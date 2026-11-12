#![no_std]
#![forbid(unsafe_code)]

//! A cleartext HTTP/2 server connection for embedded async I/O.
//!
//! This foundation buffers one bounded request at a time. It supports
//! sequential streams, and sends complete responses with optional trailers.
//! Multiplexed application streams and incremental bodies are future work.
//! Drop the connection on an error other than a reported stream reset, or
//! if an I/O future is canceled partway through a frame.

extern crate alloc;

mod headers;

use alloc::{vec, vec::Vec};
use core::cmp::min;

use embedded_io_async::{Read, ReadExactError, Write};
use zerodds_hpack::{Decoder, Encoder};
use zerodds_http2::{
    CLIENT_PREFACE, ErrorCode, Flags, FrameHeader, FrameType, Http2Error, Settings, StreamId,
    decode_frame, encode_frame,
    flow::{FlowControl, decode_window_update, encode_window_update},
    frame::{DEFAULT_MAX_FRAME_SIZE, FRAME_HEADER_LEN},
    settings::{Setting, SettingId, decode_settings, encode_settings},
    stream::is_client_initiated,
};

pub use zerodds_hpack::HeaderField;

const MAX_BODY: usize = 16 * 1024;

#[derive(Debug)]
pub enum Error<E> {
    Read(ReadExactError<E>),
    Write(E),
    Wire(Http2Error),
    Protocol,
    HeaderBlock,
    BodyLimit,
    Reset { stream_id: StreamId, code: u32 },
    Closed,
}

impl<E> From<Http2Error> for Error<E> {
    fn from(error: Http2Error) -> Self {
        Self::Wire(error)
    }
}

#[derive(Debug)]
pub struct Request {
    pub stream_id: StreamId,
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
}

pub struct Response<'a> {
    pub headers: &'a [HeaderField],
    pub body: &'a [u8],
    pub trailers: &'a [HeaderField],
}

struct Frame {
    kind: u8,
    flags: Flags,
    stream_id: StreamId,
}

struct Stream {
    id: StreamId,
    send: FlowControl,
    receive: FlowControl,
    remote_closed: bool,
}

pub struct Server<Io> {
    io: Io,
    peer: Settings,
    decoder: Decoder,
    encoder: Encoder,
    connection_send: FlowControl,
    connection_receive: FlowControl,
    payload: Vec<u8>,
    stream: Option<Stream>,
    last_stream: StreamId,
}

impl<Io: Read + Write> Server<Io> {
    /// Complete the cleartext, prior-knowledge handshake on an accepted socket.
    pub async fn handshake(io: Io) -> Result<Self, Error<Io::Error>> {
        let mut server = Self {
            io,
            peer: Settings::default(),
            decoder: Decoder::new(),
            encoder: Encoder::with_max_size(0),
            connection_send: FlowControl::default(),
            connection_receive: FlowControl::default(),
            payload: Vec::new(),
            stream: None,
            last_stream: 0,
        };
        let mut preface = [0; CLIENT_PREFACE.len()];
        server
            .io
            .read_exact(&mut preface)
            .await
            .map_err(Error::Read)?;
        if preface != *CLIENT_PREFACE {
            return Err(Http2Error::BadPreface.into());
        }
        let settings = encode_settings(&[
            Setting {
                id: SettingId::MaxConcurrentStreams,
                value: 1,
            },
            Setting {
                id: SettingId::MaxHeaderListSize,
                value: headers::MAX_LIST as u32,
            },
        ]);
        server
            .write_frame(FrameType::Settings, 0, 0, &settings)
            .await?;
        let first = server.read_frame().await?;
        if first.kind != FrameType::Settings as u8 || first.flags.has(Flags::ACK) {
            return Err(Error::Protocol);
        }
        server.control(first).await?;
        Ok(server)
    }

    /// Read a complete request or report a reset before its body completed.
    /// A successful request must be answered before reading the next one.
    pub async fn accept(&mut self) -> Result<Request, Error<Io::Error>> {
        if self.stream.is_some() {
            return Err(Error::Protocol);
        }

        let mut request = loop {
            let frame = self.read_frame().await?;
            if frame.kind != FrameType::Headers as u8 {
                self.control(frame).await?;
                continue;
            }
            if !is_client_initiated(frame.stream_id) || frame.stream_id <= self.last_stream {
                return Err(Error::Protocol);
            }
            let fields = self.read_headers(&frame).await?;
            self.last_stream = frame.stream_id;
            self.stream = Some(Stream {
                id: frame.stream_id,
                send: FlowControl::new(i64::from(self.peer.initial_window_size)),
                receive: FlowControl::default(),
                remote_closed: frame.flags.has(Flags::END_STREAM),
            });
            break Request {
                stream_id: frame.stream_id,
                headers: fields,
                body: Vec::new(),
            };
        };

        while self.stream.as_ref().is_some_and(|s| !s.remote_closed) {
            let frame = self.read_frame().await?;
            if frame.kind != FrameType::Data as u8 {
                self.control(frame).await?;
                continue;
            }
            let Some(stream) = self.stream.as_mut().filter(|s| s.id == frame.stream_id) else {
                return Err(Error::Protocol);
            };
            let data = strip_padding(&self.payload, frame.flags).ok_or(Error::Protocol)?;
            if request.body.len() + data.len() > MAX_BODY {
                return Err(Error::BodyLimit);
            }
            let credit = self.payload.len() as u32;
            self.connection_receive.consume(credit)?;
            stream.receive.consume(credit)?;
            request.body.extend_from_slice(data);
            let ended = frame.flags.has(Flags::END_STREAM);
            stream.remote_closed = ended;

            if credit > 0 {
                self.connection_receive.apply_window_update(credit)?;
                let payload = encode_window_update(credit);
                if !ended {
                    stream.receive.apply_window_update(credit)?;
                    self.write_frame(FrameType::WindowUpdate, frame.stream_id, 0, &payload)
                        .await?;
                }
                self.write_frame(FrameType::WindowUpdate, 0, 0, &payload)
                    .await?;
            }
        }
        Ok(request)
    }

    /// Send response headers, bounded DATA, then trailers when supplied.
    pub async fn respond(
        &mut self,
        stream_id: StreamId,
        response: Response<'_>,
    ) -> Result<(), Error<Io::Error>> {
        if !self
            .stream
            .as_ref()
            .is_some_and(|s| s.id == stream_id && s.remote_closed)
        {
            return Err(Error::Protocol);
        }
        if response.body.len() > MAX_BODY {
            return Err(Error::BodyLimit);
        }
        let head = self.encode_headers(response.headers)?;
        let trailers = if response.trailers.is_empty() {
            None
        } else {
            Some(self.encode_headers(response.trailers)?)
        };

        let end_in_headers = response.body.is_empty() && trailers.is_none();
        let flags = Flags::END_HEADERS | if end_in_headers { Flags::END_STREAM } else { 0 };
        self.write_frame(FrameType::Headers, stream_id, flags, &head)
            .await?;

        let mut body = response.body;
        while !body.is_empty() {
            let stream = self.stream.as_mut().ok_or(Error::Protocol)?;
            let window = min(self.connection_send.window(), stream.send.window());
            if window <= 0 {
                let frame = self.read_frame().await?;
                self.control(frame).await?;
                continue;
            }
            let len = body
                .len()
                .min(window as usize)
                .min(DEFAULT_MAX_FRAME_SIZE as usize);
            let flags = if len == body.len() && trailers.is_none() {
                Flags::END_STREAM
            } else {
                0
            };
            self.connection_send.consume(len as u32)?;
            stream.send.consume(len as u32)?;
            self.write_frame(FrameType::Data, stream_id, flags, &body[..len])
                .await?;
            body = &body[len..];
        }

        if let Some(trailers) = trailers {
            self.write_frame(
                FrameType::Headers,
                stream_id,
                Flags::END_HEADERS | Flags::END_STREAM,
                &trailers,
            )
            .await?;
        }
        self.stream = None;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Frame, Error<Io::Error>> {
        let mut header = [0; FRAME_HEADER_LEN];
        self.io.read_exact(&mut header).await.map_err(Error::Read)?;
        let len = u32::from_be_bytes([0, header[0], header[1], header[2]]);
        if len > DEFAULT_MAX_FRAME_SIZE {
            return Err(Http2Error::FrameTooLarge {
                got: len,
                max: DEFAULT_MAX_FRAME_SIZE,
            }
            .into());
        }
        self.payload.resize(FRAME_HEADER_LEN + len as usize, 0);
        self.payload[..FRAME_HEADER_LEN].copy_from_slice(&header);
        self.io
            .read_exact(&mut self.payload[FRAME_HEADER_LEN..])
            .await
            .map_err(Error::Read)?;
        let (kind, flags, stream_id) = match decode_frame(&self.payload, DEFAULT_MAX_FRAME_SIZE) {
            Ok((frame, _)) => (
                frame.header.frame_type as u8,
                frame.header.flags,
                frame.header.stream_id,
            ),
            // An unknown extension frame still consumes its complete payload.
            Err(Http2Error::UnknownFrameType(kind)) => {
                let id = u32::from_be_bytes([header[5] & 0x7f, header[6], header[7], header[8]]);
                (kind, Flags(header[4]), id)
            }
            Err(error) => return Err(error.into()),
        };
        self.payload.drain(..FRAME_HEADER_LEN);
        Ok(Frame {
            kind,
            flags,
            stream_id,
        })
    }

    async fn write_frame(
        &mut self,
        kind: FrameType,
        stream_id: StreamId,
        flags: u8,
        payload: &[u8],
    ) -> Result<(), Error<Io::Error>> {
        let header = FrameHeader {
            length: payload.len() as u32,
            frame_type: kind,
            flags: Flags(flags),
            stream_id,
        };
        let mut output = vec![0; FRAME_HEADER_LEN + payload.len()];
        let len = encode_frame(&header, payload, &mut output, DEFAULT_MAX_FRAME_SIZE)?;
        self.io
            .write_all(&output[..len])
            .await
            .map_err(Error::Write)?;
        self.io.flush().await.map_err(Error::Write)
    }

    async fn read_headers(&mut self, first: &Frame) -> Result<Vec<HeaderField>, Error<Io::Error>> {
        let mut fragment = strip_padding(&self.payload, first.flags).ok_or(Error::Protocol)?;
        if first.flags.has(Flags::PRIORITY) {
            fragment = fragment.get(5..).ok_or(Error::Protocol)?;
        }
        if fragment.len() > headers::MAX_BLOCK {
            return Err(Error::HeaderBlock);
        }
        let mut block = fragment.to_vec();
        let mut end = first.flags.has(Flags::END_HEADERS);
        while !end {
            let frame = self.read_frame().await?;
            if frame.kind != FrameType::Continuation as u8 || frame.stream_id != first.stream_id {
                return Err(Error::Protocol);
            }
            if block.len() + self.payload.len() > headers::MAX_BLOCK {
                return Err(Error::HeaderBlock);
            }
            block.extend_from_slice(&self.payload);
            end = frame.flags.has(Flags::END_HEADERS);
        }
        headers::decode(&mut self.decoder, &block).ok_or(Error::HeaderBlock)
    }

    fn encode_headers(&mut self, fields: &[HeaderField]) -> Result<Vec<u8>, Error<Io::Error>> {
        let limit = headers::MAX_LIST.min(self.peer.max_header_list_size as usize);
        let mut size = 0;
        for field in fields {
            size += field.size();
            if size > limit {
                return Err(Error::HeaderBlock);
            }
        }
        // Announce the zero-sized encoder table before sending any fields.
        let mut block = vec![0x20];
        block.extend(self.encoder.encode(fields));
        if block.len() > headers::MAX_BLOCK {
            return Err(Error::HeaderBlock);
        }
        Ok(block)
    }

    async fn control(&mut self, frame: Frame) -> Result<(), Error<Io::Error>> {
        match FrameType::from_u8(frame.kind) {
            Ok(FrameType::Settings) if frame.stream_id == 0 => {
                if frame.flags.has(Flags::ACK) {
                    return if self.payload.is_empty() {
                        Ok(())
                    } else {
                        Err(Error::Protocol)
                    };
                }
                for setting in decode_settings(&self.payload)? {
                    let old = self.peer.initial_window_size;
                    self.peer.apply(setting)?;
                    if let Some(stream) = &mut self.stream {
                        stream.send.apply_initial_window_size_change(
                            i64::from(old),
                            i64::from(self.peer.initial_window_size),
                        );
                    }
                }
                self.write_frame(FrameType::Settings, 0, Flags::ACK, &[])
                    .await
            }
            Ok(FrameType::Ping) if frame.stream_id == 0 && self.payload.len() == 8 => {
                if !frame.flags.has(Flags::ACK) {
                    let mut ping = [0; 8];
                    ping.copy_from_slice(&self.payload);
                    self.write_frame(FrameType::Ping, 0, Flags::ACK, &ping)
                        .await?;
                }
                Ok(())
            }
            Ok(FrameType::WindowUpdate) => {
                let increment = decode_window_update(&self.payload)?;
                if frame.stream_id == 0 {
                    self.connection_send.apply_window_update(increment)?;
                } else if let Some(stream) =
                    self.stream.as_mut().filter(|s| s.id == frame.stream_id)
                {
                    stream.send.apply_window_update(increment)?;
                } else if frame.stream_id > self.last_stream {
                    return Err(Error::Protocol);
                }
                Ok(())
            }
            Ok(FrameType::RstStream) if frame.stream_id != 0 && self.payload.len() == 4 => {
                let code =
                    u32::from_be_bytes(self.payload[..].try_into().map_err(|_| Error::Protocol)?);
                if self
                    .stream
                    .as_ref()
                    .is_some_and(|s| s.id == frame.stream_id)
                {
                    self.stream = None;
                    return Err(Error::Reset {
                        stream_id: frame.stream_id,
                        code,
                    });
                }
                if frame.stream_id > self.last_stream {
                    return Err(Error::Protocol);
                }
                Ok(())
            }
            Ok(FrameType::GoAway) if frame.stream_id == 0 && self.payload.len() >= 8 => {
                Err(Error::Closed)
            }
            Ok(FrameType::Priority) if frame.stream_id != 0 && self.payload.len() == 5 => Ok(()),
            Err(Http2Error::UnknownFrameType(_)) => Ok(()),
            _ => Err(Http2Error::Protocol(ErrorCode::ProtocolError).into()),
        }
    }
}

fn strip_padding(payload: &[u8], flags: Flags) -> Option<&[u8]> {
    if !flags.has(Flags::PADDED) {
        return Some(payload);
    }
    let (&padding, data) = payload.split_first()?;
    data.get(..data.len().checked_sub(usize::from(padding))?)
}
