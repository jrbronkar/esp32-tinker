#![no_std]
#![forbid(unsafe_code)]

//! Identity-encoded unary and streaming gRPC over embedded HTTP/2.
//! The streaming runtime uses an independently driven local connection.

extern crate alloc;

mod framing;
mod streaming;

pub use embedded_http2::{Config as Http2Config, server::StreamError};
pub use futures_core::Stream;
pub use streaming::{
    Request, Response, ServerStreaming, Streaming, StreamingCall, StreamingService,
    serve_streaming, serve_streaming_with_config, serve_streaming_with_timer,
};

pub use framing::MessageDecoder;

use alloc::{vec, vec::Vec};
use embedded_http2::{
    HeaderField,
    service::{Request as HttpRequest, Response as HttpResponse},
};
use embedded_io_async::{Read, ReadExactError, Write};
use framing::{ENVELOPE_LEN, UNCOMPRESSED, decode_message, message_length};
use micropb::{MessageDecode, MessageEncode, PbEncoder};

/// Dispatches a buffered unary RPC. Generated servers implement this trait.
#[allow(async_fn_in_trait)]
pub trait Service {
    async fn call(
        &mut self,
        path: &str,
        request: UnaryRequest<'_>,
    ) -> Result<UnaryResponse, Status>;
}

/// Failure while accepting or serving a gRPC connection.
#[derive(Debug)]
pub struct ConnectionError<E>(embedded_http2::Error<E>);

impl<E> ConnectionError<E> {
    /// Whether the peer sent GOAWAY or the byte stream ended.
    pub fn is_disconnected(&self) -> bool {
        matches!(
            self.0,
            embedded_http2::Error::Read(ReadExactError::UnexpectedEof)
                | embedded_http2::Error::Closed
        )
    }
}

/// Serve sequential gRPC calls on an accepted cleartext socket.
///
/// Run on a local executor with cancellation-safe socket reads.
pub async fn serve<Io: Read + Write, S: Service>(
    io: Io,
    service: &mut S,
) -> Result<(), ConnectionError<Io::Error>> {
    embedded_http2::service::serve(io, &mut HttpService(service))
        .await
        .map_err(ConnectionError)
}

struct HttpService<'a, S>(&'a mut S);

impl<S: Service> embedded_http2::service::Service for HttpService<'_, S> {
    async fn call(&mut self, request: HttpRequest) -> HttpResponse {
        let path = match rpc_path(&request.headers) {
            Ok(path) => path,
            Err(response) => return response,
        };
        match self.0.call(path, UnaryRequest(&request.body)).await {
            Ok(response) => grpc_response(response.0, GRPC_OK),
            Err(status) => status.into_response(),
        }
    }
}

const CONTENT_TYPE: &str = "application/grpc";
const HTTP_OK: &str = "200";
const HTTP_METHOD_NOT_ALLOWED: &str = "405";
const HTTP_UNSUPPORTED_MEDIA_TYPE: &str = "415";
const GRPC_OK: &str = "0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InvalidArgument,
    DeadlineExceeded,
    ResourceExhausted,
    FailedPrecondition,
    Unimplemented,
    Internal,
    Unavailable,
}

impl Status {
    fn wire_code(self) -> &'static str {
        match self {
            Self::InvalidArgument => "3",
            Self::DeadlineExceeded => "4",
            Self::ResourceExhausted => "8",
            Self::FailedPrecondition => "9",
            Self::Unimplemented => "12",
            Self::Internal => "13",
            Self::Unavailable => "14",
        }
    }

    fn into_response(self) -> HttpResponse {
        grpc_response(Vec::new(), self.wire_code())
    }
}

/// Check the gRPC HTTP headers and return the RPC path.
fn rpc_path(headers: &[HeaderField]) -> Result<&str, HttpResponse> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.as_str())
    };
    if !header("content-type").is_some_and(|value| {
        value.strip_prefix(CONTENT_TYPE).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with("+") || suffix.starts_with(";")
        })
    }) {
        return Err(http_error(HTTP_UNSUPPORTED_MEDIA_TYPE));
    }
    if header(":method") != Some("POST") {
        let mut response = http_error(HTTP_METHOD_NOT_ALLOWED);
        response.headers.push(field("allow", "POST"));
        return Err(response);
    }
    if header("grpc-encoding").is_some_and(|encoding| encoding != "identity") {
        let mut response = Status::Unimplemented.into_response();
        response
            .headers
            .push(field("grpc-accept-encoding", "identity"));
        return Err(response);
    }
    header(":path").ok_or_else(|| Status::InvalidArgument.into_response())
}

/// A single buffered gRPC message awaiting typed decoding.
pub struct UnaryRequest<'a>(&'a [u8]);

impl UnaryRequest<'_> {
    pub fn decode<M: MessageDecode + Default, const MAX: usize>(self) -> Result<M, Status> {
        let (header, body) = self
            .0
            .split_first_chunk::<ENVELOPE_LEN>()
            .ok_or(Status::InvalidArgument)?;
        if message_length::<MAX>(*header)? != body.len() {
            return Err(Status::InvalidArgument);
        }
        decode_message(body)
    }
}

/// An encoded unary reply ready for the gRPC runtime.
pub struct UnaryResponse(Vec<u8>);

impl UnaryResponse {
    pub fn encode<M: MessageEncode, const MAX: usize>(message: M) -> Result<Self, Status> {
        let mut buffer = [0; MAX];
        let mut encoder = PbEncoder::new(buffer.as_mut_slice());
        message.encode(&mut encoder).map_err(|_| Status::Internal)?;
        // The slice writer advances past the encoded bytes on every write.
        let written = MAX - encoder.into_writer().len();
        let bytes = &buffer[..written];
        let len = u32::try_from(written).map_err(|_| Status::Internal)?;
        let mut body = Vec::new();
        body.try_reserve_exact(ENVELOPE_LEN + bytes.len())
            .map_err(|_| Status::ResourceExhausted)?;
        body.push(UNCOMPRESSED);
        body.extend_from_slice(&len.to_be_bytes());
        body.extend_from_slice(bytes);
        Ok(Self(body))
    }
}

fn grpc_response(body: Vec<u8>, code: &str) -> HttpResponse {
    let mut response = http_error(HTTP_OK);
    response.headers.push(field("content-type", CONTENT_TYPE));
    response.body = body;
    response.trailers.push(field("grpc-status", code));
    response
}

fn http_error(code: &str) -> HttpResponse {
    HttpResponse {
        headers: vec![field(":status", code)],
        body: Vec::new(),
        trailers: Vec::new(),
    }
}

fn field(name: &str, value: &str) -> HeaderField {
    HeaderField {
        name: name.into(),
        value: value.into(),
    }
}
