#![no_std]
#![forbid(unsafe_code)]

//! Buffered unary gRPC over the embedded HTTP service boundary.
//! Supports identity encoding. Streaming and deadlines require an
//! independently driven connection and are outside this unary adapter.

extern crate alloc;

use alloc::{vec, vec::Vec};
use embedded_http2::HeaderField;
use micropb::{MessageDecode, MessageEncode, PbEncoder};

pub use embedded_http2::service::{Request as HttpRequest, Response as HttpResponse, Service};

const CONTENT_TYPE: &str = "application/grpc";
const ENVELOPE_LEN: usize = 1 + size_of::<u32>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InvalidArgument,
    ResourceExhausted,
    Unimplemented,
    Internal,
    Unavailable,
}

impl Status {
    fn wire_code(self) -> &'static str {
        match self {
            Self::InvalidArgument => "3",
            Self::ResourceExhausted => "8",
            Self::Unimplemented => "12",
            Self::Internal => "13",
            Self::Unavailable => "14",
        }
    }

    pub fn into_response(self) -> HttpResponse {
        grpc_response(Vec::new(), self.wire_code())
    }
}

/// Check the gRPC HTTP headers and return the RPC path.
pub fn rpc_path(request: &HttpRequest) -> Result<&str, HttpResponse> {
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.as_str())
    };
    if !header("content-type").is_some_and(|value| {
        value.strip_prefix(CONTENT_TYPE).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with("+") || suffix.starts_with(";")
        })
    }) {
        return Err(http_error("415"));
    }
    if header(":method") != Some("POST") {
        let mut response = http_error("405");
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

pub fn decode_unary<M: MessageDecode + Default, const MAX: usize>(
    request: &HttpRequest,
) -> Result<M, Status> {
    let (head, body) = request
        .body
        .split_at_checked(ENVELOPE_LEN)
        .ok_or(Status::InvalidArgument)?;
    match head[0] {
        0 => {}
        1 => return Err(Status::Unimplemented),
        _ => return Err(Status::InvalidArgument),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX {
        return Err(Status::ResourceExhausted);
    }
    if len != body.len() {
        return Err(Status::InvalidArgument);
    }
    let mut message = M::default();
    message
        .decode_from_bytes(body)
        .map_err(|_| Status::InvalidArgument)?;
    Ok(message)
}

pub fn unary_response<M: MessageEncode, const MAX: usize>(
    result: Result<M, Status>,
) -> HttpResponse {
    let encode = |message: M| {
        let mut encoder = PbEncoder::new(heapless::Vec::<u8, MAX>::new());
        message.encode(&mut encoder).map_err(|_| Status::Internal)?;
        let bytes = encoder.into_writer();
        let len = u32::try_from(bytes.len()).map_err(|_| Status::Internal)?;
        let mut body = Vec::with_capacity(ENVELOPE_LEN + bytes.len());
        body.push(0);
        body.extend_from_slice(&len.to_be_bytes());
        body.extend_from_slice(&bytes);
        Ok(body)
    };
    match result.and_then(encode) {
        Ok(body) => grpc_response(body, "0"),
        Err(status) => status.into_response(),
    }
}

fn grpc_response(body: Vec<u8>, code: &str) -> HttpResponse {
    let mut response = http_error("200");
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
