use alloc::vec::Vec;

use embedded_io_async::{Read, Write};
use zerodds_http2::{Flags, FrameType, StreamId, frame::DEFAULT_MAX_FRAME_SIZE};

use crate::{BodyFrame, Error, HeaderField, MAX_BODY, NO_FLAGS, Response, Server};

/// Available send credit or an inbound frame received while waiting for credit.
#[derive(Debug)]
pub enum SendEvent<'a> {
    Ready,
    RequestBody(BodyFrame<'a>),
}

impl<Io: Read + Write> Server<Io> {
    /// Send initial response headers. The request may still be open.
    pub async fn start_response(
        &mut self,
        stream_id: StreamId,
        headers: &[HeaderField],
    ) -> Result<(), Error<Io::Error>> {
        self.check_response_start(stream_id, false)?;
        let head = self.encode_headers(headers)?;
        self.write_response_head(stream_id, &head, false).await
    }

    /// Send up to one DATA frame, returning how many bytes were sent.
    ///
    /// A nonempty slice can make zero progress when either send window is
    /// exhausted. Use [Self::wait_send_capacity] before retrying the remainder.
    pub async fn send_data(
        &mut self,
        stream_id: StreamId,
        bytes: &[u8],
    ) -> Result<usize, Error<Io::Error>> {
        self.send_data_frame(stream_id, bytes, false).await
    }

    /// Wait for send credit or yield an inbound body frame to the caller.
    ///
    /// Process a returned request frame and release its receive credit before
    /// waiting again. This lets the caller service the request while a response
    /// is blocked, without a second reader or an unbounded queue.
    pub async fn wait_send_capacity(
        &mut self,
        stream_id: StreamId,
    ) -> Result<SendEvent<'_>, Error<Io::Error>> {
        while self.send_capacity(stream_id)? == 0 {
            if let Some(frame) = self.receive_body_frame(stream_id).await? {
                return self.body_frame(frame).map(SendEvent::RequestBody);
            }
        }
        Ok(SendEvent::Ready)
    }

    /// End the response after the request has half-closed.
    ///
    /// With no trailers, sends an empty DATA frame with END_STREAM.
    pub async fn finish_response(
        &mut self,
        stream_id: StreamId,
        trailers: &[HeaderField],
    ) -> Result<(), Error<Io::Error>> {
        if !self.stream.as_ref().is_some_and(|stream| {
            stream.id == stream_id && stream.response_started && stream.remote_closed
        }) {
            return Err(Error::Protocol);
        }
        let trailers = self.encode_response_trailers(trailers)?;
        self.release_capacity(stream_id).await?;
        if let Some(trailers) = trailers {
            self.write_response_trailers(stream_id, &trailers).await?;
        } else {
            self.write_frame(FrameType::Data, stream_id, Flags::END_STREAM, &[])
                .await?;
        }
        self.stream = None;
        Ok(())
    }

    /// Send response headers, bounded DATA, then trailers when supplied.
    pub async fn respond(
        &mut self,
        stream_id: StreamId,
        response: Response<'_>,
    ) -> Result<(), Error<Io::Error>> {
        self.check_response_start(stream_id, true)?;
        if response.body.len() > MAX_BODY {
            return Err(Error::BodyLimit);
        }
        let head = self.encode_headers(response.headers)?;
        let trailers = self.encode_response_trailers(response.trailers)?;

        self.release_capacity(stream_id).await?;
        let end_in_headers = response.body.is_empty() && trailers.is_none();
        self.write_response_head(stream_id, &head, end_in_headers)
            .await?;

        let mut body = response.body;
        while !body.is_empty() {
            let sent = self
                .send_data_frame(stream_id, body, trailers.is_none())
                .await?;
            if sent == 0 {
                let frame = self.read_frame().await?;
                self.control(frame).await?;
            }
            body = body.get(sent..).ok_or(Error::Protocol)?;
        }

        if let Some(trailers) = trailers {
            self.write_response_trailers(stream_id, &trailers).await?;
        }
        self.stream = None;
        Ok(())
    }

    fn check_response_start(
        &self,
        stream_id: StreamId,
        require_remote_close: bool,
    ) -> Result<(), Error<Io::Error>> {
        if self.stream.as_ref().is_some_and(|stream| {
            stream.id == stream_id
                && !stream.response_started
                && (!require_remote_close || stream.remote_closed)
        }) {
            Ok(())
        } else {
            Err(Error::Protocol)
        }
    }

    async fn write_response_head(
        &mut self,
        stream_id: StreamId,
        head: &[u8],
        end_stream: bool,
    ) -> Result<(), Error<Io::Error>> {
        let flags = Flags::END_HEADERS
            | if end_stream {
                Flags::END_STREAM
            } else {
                NO_FLAGS
            };
        self.write_frame(FrameType::Headers, stream_id, flags, head)
            .await?;
        self.stream
            .as_mut()
            .ok_or(Error::Protocol)?
            .response_started = true;
        Ok(())
    }

    fn encode_response_trailers(
        &mut self,
        trailers: &[HeaderField],
    ) -> Result<Option<Vec<u8>>, Error<Io::Error>> {
        if trailers.is_empty() {
            Ok(None)
        } else {
            self.encode_headers(trailers).map(Some)
        }
    }

    async fn write_response_trailers(
        &mut self,
        stream_id: StreamId,
        trailers: &[u8],
    ) -> Result<(), Error<Io::Error>> {
        self.write_frame(
            FrameType::Headers,
            stream_id,
            Flags::END_HEADERS | Flags::END_STREAM,
            trailers,
        )
        .await
    }

    fn send_capacity(&self, stream_id: StreamId) -> Result<usize, Error<Io::Error>> {
        let stream = self
            .stream
            .as_ref()
            .filter(|stream| stream.id == stream_id && stream.response_started)
            .ok_or(Error::Protocol)?;
        let window = self.connection_send.window().min(stream.send.window());
        Ok(window.clamp(0, i64::from(DEFAULT_MAX_FRAME_SIZE)) as usize)
    }

    async fn send_data_frame(
        &mut self,
        stream_id: StreamId,
        bytes: &[u8],
        end_stream: bool,
    ) -> Result<usize, Error<Io::Error>> {
        let len = bytes.len().min(self.send_capacity(stream_id)?);
        if len == 0 {
            return Ok(0);
        }
        let flags = if end_stream && len == bytes.len() {
            Flags::END_STREAM
        } else {
            NO_FLAGS
        };
        let chunk = bytes.get(..len).ok_or(Error::Protocol)?;
        let stream = self.stream.as_mut().ok_or(Error::Protocol)?;
        self.connection_send.consume(len as u32)?;
        stream.send.consume(len as u32)?;
        self.write_frame(FrameType::Data, stream_id, flags, chunk)
            .await?;
        Ok(len)
    }
}
