use embedded_io_async::{Read, Write};
use zerodds_http2::{ErrorCode, Flags, FrameType, StreamId};

use crate::{BodyFrame, Error, RequestHead, Server};

const MAX_STREAMS: usize = 2;

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
    /// Accept up to two active streams, driven by one caller using [Self::next_event].
    ///
    /// The transport retains no queued bodies. DATA borrows the receive buffer,
    /// and the caller returns receive credit after consuming it. The caller
    /// owns pending response bytes and chooses which stream to write next.
    pub async fn handshake(io: Io) -> Result<Self, Error<Io::Error>> {
        Self::handshake_with_limit(io, MAX_STREAMS).await
    }

    /// Read one inbound event. Use the incremental response methods to send
    /// on any active stream. A zero-byte send leaves the caller free to
    /// advance other streams before reading another event.
    pub async fn next_event(&mut self) -> Result<Event<'_>, Error<Io::Error>> {
        if self.max_streams == 1 {
            return Err(Error::Protocol);
        }
        let frame = self.read_frame().await?;
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
                        Ok(Some(frame)) => {
                            return Ok(Event::Body {
                                stream_id,
                                frame: self.body_frame(frame)?,
                                end_stream,
                            });
                        }
                        Ok(None) => Ok(()),
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

    pub(super) fn require_sequential(&self) -> Result<(), Error<Io::Error>> {
        if self.max_streams == 1 {
            Ok(())
        } else {
            Err(Error::Protocol)
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
        if frame.kind == FrameType::Headers as u8 {
            // Keep the connection-wide HPACK decoder in sync.
            self.read_headers(&frame).await?;
        } else {
            let credit = self.payload.len() as u32;
            self.connection_receive.consume(credit)?;
            self.release_connection_capacity(credit).await?;
        }
        let code = ErrorCode::StreamClosed;
        if self.stream(frame.stream_id).is_some() {
            self.reset_stream(frame.stream_id, code).await?;
            return Err(Error::Reset {
                stream_id: frame.stream_id,
                code: code as u32,
            });
        }
        Ok(())
    }
}
