use embedded_io_async::{Read, Write};
use zerodds_http2::{ErrorCode, Flags, FrameType, StreamId};

use crate::{BodyFrame, Error, RequestHead, Server};

/// One inbound event on a multiplexed connection.
#[derive(Debug)]
pub enum Event<'a> {
    Request(RequestHead),
    Body {
        stream_id: StreamId,
        frame: BodyFrame<'a>,
        end_stream: bool,
    },
    Reset {
        stream_id: StreamId,
        code: u32,
    },
    /// A control frame was handled. Retry pending writes after this event.
    Control,
}

impl<Io: Read + Write> Server<Io> {
    /// Read one inbound event. Use the incremental response methods to send
    /// on any active stream. A zero-byte send leaves the caller free to
    /// advance other streams before reading another event.
    pub async fn next_event(&mut self) -> Result<Event<'_>, Error<Io::Error>> {
        let frame = self.read_inbound().await?;
        self.dispatch(frame).await
    }

    pub(super) async fn dispatch(
        &mut self,
        frame: crate::Frame,
    ) -> Result<Event<'_>, Error<Io::Error>> {
        let stream_id = frame.stream_id;
        let end_stream = frame.flags.has(Flags::END_STREAM);
        let result = match FrameType::from_u8(frame.kind) {
            Ok(FrameType::Headers) if stream_id > self.last_stream => {
                match self.open_request(&frame).await {
                    Ok(request) => return Ok(Event::Request(request)),
                    Err(error) => Err(error),
                }
            }
            Ok(FrameType::Data | FrameType::Headers)
                if stream_id != 0 && stream_id <= self.last_stream =>
            {
                if self
                    .stream(stream_id)
                    .is_some_and(|stream| !stream.remote_closed)
                {
                    match self.process_body_frame(stream_id, frame).await {
                        Ok(frame) => {
                            return Ok(Event::Body {
                                stream_id,
                                frame: self.body_frame(frame)?,
                                end_stream,
                            });
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    self.discard_closed_body(frame).await
                }
            }
            _ => self.control(frame).await,
        };
        match result {
            Ok(()) => Ok(Event::Control),
            Err(Error::Reset { stream_id, code }) => Ok(Event::Reset { stream_id, code }),
            Err(Error::MalformedRequest) => Ok(Event::Reset {
                stream_id,
                code: ErrorCode::ProtocolError as u32,
            }),
            Err(error) => Err(error),
        }
    }

    pub(super) async fn reset_stream(
        &mut self,
        stream_id: StreamId,
        code: ErrorCode,
    ) -> Result<(), Error<Io::Error>> {
        self.write_reset(stream_id, code).await?;
        if let Some(stream) = self.remove_stream(stream_id) {
            self.release_connection_capacity(stream.pending_receive_credit)
                .await?;
        }
        Ok(())
    }

    async fn discard_closed_body(&mut self, frame: crate::Frame) -> Result<(), Error<Io::Error>> {
        if frame.kind == FrameType::Headers as u8 && self.stream(frame.stream_id).is_none() {
            let code = if frame.stream_id == self.last_stream {
                ErrorCode::StreamClosed
            } else {
                ErrorCode::ProtocolError
            };
            self.write_goaway(code).await?;
            return Err(Error::Protocol);
        }
        let credit = if frame.kind == FrameType::Headers as u8 {
            // Keep the connection-wide HPACK decoder in sync.
            self.decode_headers()?;
            0
        } else {
            let credit = self.payload.len() as u32;
            self.connection_receive.consume(credit)?;
            credit
        };
        let code = ErrorCode::StreamClosed;
        self.reset_stream(frame.stream_id, code).await?;
        self.release_connection_capacity(credit).await?;
        Err(Error::Reset {
            stream_id: frame.stream_id,
            code: code as u32,
        })
    }
}
