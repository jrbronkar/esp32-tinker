use alloc::vec::Vec;
use core::mem;

use embedded_io_async::{Read, Write};
use zerodds_http2::{
    Flags, FrameType, StreamId, flow::encode_window_update, stream::is_client_initiated,
};

use crate::{Error, FlowControl, HeaderField, MAX_BODY, Request, Server, Stream, strip_padding};

const CONNECTION_STREAM_ID: StreamId = 0;
const NO_FLAGS: u8 = 0;

/// Initial headers for the connection's active request.
#[derive(Debug)]
pub struct RequestHead {
    pub stream_id: StreamId,
    pub headers: Vec<HeaderField>,
    pub end_stream: bool,
}

/// One frame of the request body. DATA borrows the connection's frame buffer.
#[derive(Debug)]
pub enum BodyFrame<'a> {
    Data(&'a [u8]),
    Trailers(Vec<HeaderField>),
}

impl<Io: Read + Write> Server<Io> {
    /// Receive the initial headers without waiting for the request body.
    pub async fn accept_head(&mut self) -> Result<RequestHead, Error<Io::Error>> {
        if self.stream.is_some() {
            return Err(Error::Protocol);
        }
        loop {
            let frame = self.read_frame().await?;
            if frame.kind != FrameType::Headers as u8 {
                self.control(frame).await?;
                continue;
            }
            if !is_client_initiated(frame.stream_id) || frame.stream_id <= self.last_stream {
                return Err(Error::Protocol);
            }

            let headers = self.read_headers(&frame).await?;
            let end_stream = frame.flags.has(Flags::END_STREAM);
            self.last_stream = frame.stream_id;
            self.stream = Some(Stream {
                id: frame.stream_id,
                send: FlowControl::new(i64::from(self.peer.initial_window_size)),
                receive: FlowControl::default(),
                remote_closed: end_stream,
                pending_receive_credit: 0,
            });
            return Ok(RequestHead {
                stream_id: frame.stream_id,
                headers,
                end_stream,
            });
        }
    }

    /// Read the next request body frame. Returns `None` after the remote half closes.
    ///
    /// Call [Self::release_capacity] after processing DATA to let the peer send
    /// more. DATA remains valid until the next operation on the connection.
    pub async fn read_body(
        &mut self,
        stream_id: StreamId,
    ) -> Result<Option<BodyFrame<'_>>, Error<Io::Error>> {
        let stream = self
            .stream
            .as_ref()
            .filter(|stream| stream.id == stream_id)
            .ok_or(Error::Protocol)?;
        if stream.remote_closed {
            return Ok(None);
        }

        loop {
            let frame = self.read_frame().await?;
            let end_stream = frame.flags.has(Flags::END_STREAM);
            match FrameType::from_u8(frame.kind) {
                Ok(FrameType::Headers) => {
                    if frame.stream_id != stream_id || !end_stream {
                        return Err(Error::Protocol);
                    }
                    let trailers = self.read_headers(&frame).await?;
                    if trailers.iter().any(|field| field.name.starts_with(':')) {
                        return Err(Error::Protocol);
                    }
                    self.stream.as_mut().ok_or(Error::Protocol)?.remote_closed = true;
                    return Ok(Some(BodyFrame::Trailers(trailers)));
                }
                Ok(FrameType::Data) => {
                    if frame.stream_id != stream_id {
                        return Err(Error::Protocol);
                    }
                    let data = strip_padding(&self.payload, frame.flags).ok_or(Error::Protocol)?;
                    let stream = self.stream.as_mut().ok_or(Error::Protocol)?;
                    // RFC 9113 §6.1 charges the whole DATA payload, including padding.
                    let credit = self.payload.len() as u32;
                    self.connection_receive.consume(credit)?;
                    stream.receive.consume(credit)?;
                    // The receive window bounds the accumulated credit.
                    stream.pending_receive_credit += credit;
                    stream.remote_closed = end_stream;
                    return Ok(Some(BodyFrame::Data(data)));
                }
                _ => self.control(frame).await?,
            }
        }
    }

    /// Return all DATA credit accrued by the active stream since the previous release.
    ///
    /// Releasing after the remote half closes restores only connection credit.
    /// [Self::respond] also releases any credit still pending when it answers.
    pub async fn release_capacity(&mut self, stream_id: StreamId) -> Result<(), Error<Io::Error>> {
        let stream = self
            .stream
            .as_mut()
            .filter(|stream| stream.id == stream_id)
            .ok_or(Error::Protocol)?;
        let credit = mem::take(&mut stream.pending_receive_credit);
        if credit > 0 && !stream.remote_closed {
            stream.receive.apply_window_update(credit)?;
            let payload = encode_window_update(credit);
            self.write_frame(FrameType::WindowUpdate, stream_id, NO_FLAGS, &payload)
                .await?;
        }
        self.release_connection_capacity(credit).await
    }

    pub(super) async fn release_connection_capacity(
        &mut self,
        credit: u32,
    ) -> Result<(), Error<Io::Error>> {
        if credit > 0 {
            self.connection_receive.apply_window_update(credit)?;
            let payload = encode_window_update(credit);
            self.write_frame(
                FrameType::WindowUpdate,
                CONNECTION_STREAM_ID,
                NO_FLAGS,
                &payload,
            )
            .await?;
        }
        Ok(())
    }

    /// Read a complete bounded request or report a reset before its body completed.
    /// A successful request must be answered before reading the next one.
    pub async fn accept(&mut self) -> Result<Request, Error<Io::Error>> {
        let RequestHead {
            stream_id, headers, ..
        } = self.accept_head().await?;
        let mut request = Request {
            stream_id,
            headers,
            body: Vec::new(),
            trailers: Vec::new(),
        };
        while let Some(frame) = self.read_body(stream_id).await? {
            match frame {
                BodyFrame::Data(bytes) => {
                    if request.body.len() + bytes.len() > MAX_BODY {
                        return Err(Error::BodyLimit);
                    }
                    request.body.extend_from_slice(bytes);
                    self.release_capacity(stream_id).await?;
                }
                BodyFrame::Trailers(trailers) => request.trailers = trailers,
            }
        }
        Ok(request)
    }
}
