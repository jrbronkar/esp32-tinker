//! Buffered HTTP exchanges over a multiplexed HTTP/2 connection.

use alloc::vec::Vec;
use embedded_io_async::{Read, Write};
use zerodds_http2::{ErrorCode, StreamId};

use crate::{BodyFrame, Error, Event, HeaderField, MAX_BODY, MAX_STREAMS, Server};

pub struct Request {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

pub struct Response {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

/// Handles one complete request at a time. The caller supplies the executor.
#[allow(async_fn_in_trait)]
pub trait Service {
    async fn call(&mut self, request: Request) -> Response;
}

struct Exchange {
    id: StreamId,
    state: ExchangeState,
}

enum ExchangeState {
    Receiving(Request),
    Sending { response: Response, sent: usize },
}

impl<Io: Read + Write> Server<Io> {
    /// Serve buffered requests on the multiplexed connection. The adapter
    /// calls one handler at a time and retains at most one bounded buffer
    /// per active stream.
    pub async fn serve<S: Service>(&mut self, service: &mut S) -> Result<(), Error<Io::Error>> {
        let mut exchanges = Vec::with_capacity(MAX_STREAMS);
        loop {
            let mut progress = false;
            let mut index = 0;
            while let Some(exchange) = exchanges.get_mut(index) {
                let Exchange { id, state } = exchange;
                if let ExchangeState::Sending { response, sent } = state {
                    let remaining = &response.body[*sent..];
                    if !remaining.is_empty() {
                        let count = self
                            .send_data_frame(*id, remaining, response.trailers.is_empty())
                            .await?;
                        *sent += count;
                        progress |= count > 0;
                    }
                    if *sent == response.body.len() {
                        if response.trailers.is_empty() {
                            self.remove_stream(*id);
                        } else {
                            self.finish_response(*id, &response.trailers).await?;
                        }
                        exchanges.swap_remove(index);
                        continue;
                    }
                }
                index += 1;
            }
            if progress {
                continue;
            }

            match self.next_event().await? {
                Event::Request(head) => {
                    let id = head.stream_id;
                    let request = Request {
                        headers: head.headers,
                        body: Vec::new(),
                        trailers: Vec::new(),
                    };
                    let state = if head.end_stream {
                        self.call_buffered(id, service, request).await?
                    } else {
                        Some(ExchangeState::Receiving(request))
                    };
                    if let Some(state) = state {
                        exchanges.push(Exchange { id, state });
                    }
                }
                Event::Body {
                    stream_id,
                    frame,
                    end_stream,
                } => {
                    let index = exchanges
                        .iter()
                        .position(|exchange| exchange.id == stream_id)
                        .ok_or(Error::Protocol)?;
                    let ExchangeState::Receiving(request) = &mut exchanges[index].state else {
                        return Err(Error::Protocol);
                    };
                    let has_data = match frame {
                        BodyFrame::Data(data) => {
                            if request.body.len() + data.len() > MAX_BODY {
                                exchanges.swap_remove(index);
                                self.reset_stream(stream_id, ErrorCode::Cancel).await?;
                                continue;
                            }
                            request.body.extend_from_slice(data);
                            true
                        }
                        BodyFrame::Trailers(trailers) => {
                            request.trailers = trailers;
                            false
                        }
                    };
                    if has_data {
                        self.release_capacity(stream_id).await?;
                    }
                    if end_stream {
                        let exchange = exchanges.swap_remove(index);
                        let ExchangeState::Receiving(request) = exchange.state else {
                            return Err(Error::Protocol);
                        };
                        let state = self.call_buffered(stream_id, service, request).await?;
                        if let Some(state) = state {
                            exchanges.push(Exchange {
                                id: stream_id,
                                state,
                            });
                        }
                    }
                }
                Event::Reset { stream_id, .. } => {
                    exchanges.retain(|exchange| exchange.id != stream_id);
                }
                Event::Control => {}
            }
        }
    }

    async fn call_buffered<S: Service>(
        &mut self,
        stream_id: StreamId,
        service: &mut S,
        request: Request,
    ) -> Result<Option<ExchangeState>, Error<Io::Error>> {
        let response = service.call(request).await;
        if response.body.len() > MAX_BODY {
            return Err(Error::BodyLimit);
        }
        if response.body.is_empty() && response.trailers.is_empty() {
            self.respond(
                stream_id,
                crate::Response {
                    headers: &response.headers,
                    body: &[],
                    trailers: &[],
                },
            )
            .await?;
            return Ok(None);
        }
        if !response.trailers.is_empty() {
            crate::request_fields::check_trailers(&response.trailers)
                .map_err(|_| Error::Protocol)?;
        }
        self.start_response(stream_id, &response.headers).await?;
        Ok(Some(ExchangeState::Sending { response, sent: 0 }))
    }
}
