use alloc::vec;
use core::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};
use embedded_http2::server::{self, StreamError};
use embedded_io_async::{Read, Write};
use futures_core::Stream;
use micropb::{MessageDecode, MessageEncode};

use crate::{
    CONTENT_TYPE, ConnectionError, GRPC_OK, HTTP_OK, Http2Config, MessageDecoder, Status,
    UnaryResponse, field, rpc_path,
};

/// A service that can handle multiple calls on the same local executor.
#[allow(async_fn_in_trait)]
pub trait StreamingService {
    async fn call(&self, path: &str, call: StreamingCall<'_>) -> Result<(), StreamError>;
}

/// Serve with the default HTTP/2 configuration and two inline handler slots.
///
/// Client timeout syntax is validated, but expiration requires
/// [`serve_streaming_with_timer`].
pub async fn serve_streaming<Io: Read + Write, S: StreamingService>(
    io: Io,
    service: &S,
) -> Result<(), ConnectionError<Io::Error>> {
    serve_streaming_with_config::<2, _, _>(io, service, Http2Config::default()).await
}

/// Serve gRPC with `SLOTS` inline handler futures per connection.
///
/// The HTTP/2 stream limit must fit within that capacity or the call fails
/// before the handshake. Client timeout syntax is validated, but expiration
/// requires [`serve_streaming_with_timer`].
pub async fn serve_streaming_with_config<
    const SLOTS: usize,
    Io: Read + Write,
    S: StreamingService,
>(
    io: Io,
    service: &S,
    config: Http2Config,
) -> Result<(), ConnectionError<Io::Error>> {
    serve_streaming_with_timer::<SLOTS, _, _, _>(io, service, config, |_| core::future::pending())
        .await
}

/// Serve with `SLOTS` inline handler futures and enforce client deadlines.
///
/// The stream limit must fit within the handler capacity. Run on one local
/// executor with cancellation-safe reads. The timer factory receives a
/// relative [`Duration`] and must return a future that wakes when it expires.
/// Deadline-aware call operations poll it while awaiting work.
pub async fn serve_streaming_with_timer<
    const SLOTS: usize,
    Io: Read + Write,
    S: StreamingService,
    Sleep: Future<Output = ()>,
>(
    io: Io,
    service: &S,
    config: Http2Config,
    sleep: impl Fn(Duration) -> Sleep,
) -> Result<(), ConnectionError<Io::Error>> {
    let sleep = &sleep;
    embedded_http2::service::serve_streams_with_config::<SLOTS, _, _>(
        io,
        config,
        |request| async move {
            // A reset or failed response ends its own RPC. The driver reports
            // connection errors independently.
            let _ = dispatch(service, request, sleep).await;
            Ok(())
        },
    )
    .await
    .map_err(ConnectionError)
}

async fn dispatch<S: StreamingService, Sleep: Future<Output = ()>>(
    service: &S,
    request: server::Request,
    sleep: &impl Fn(Duration) -> Sleep,
) -> Result<(), StreamError> {
    let server::Request {
        head,
        mut body,
        respond,
    } = request;
    match rpc_path(&head.headers) {
        Ok(path) => {
            let timeout = match request_timeout(&head.headers) {
                Ok(timeout) => timeout,
                Err(status) => {
                    drop(body);
                    return ServerStreaming {
                        respond,
                        deadline: None,
                    }
                    .reject(status)
                    .await;
                }
            };
            let mut timer = pin!(async {
                match timeout {
                    Some(duration) => sleep(duration).await,
                    None => core::future::pending().await,
                }
            });
            let mut poll_timer = |cx: &mut Context<'_>| timer.as_mut().poll(cx);
            service
                .call(
                    path,
                    StreamingCall {
                        body,
                        respond,
                        deadline: timeout.map(|_| &mut poll_timer as &mut DeadlinePoll<'_>),
                    },
                )
                .await
        }
        Err(error) => {
            while body.data().await?.is_some() {}
            if error.trailers.is_empty() {
                respond.finish_headers(error.headers).await
            } else {
                respond
                    .send_response(error.headers)?
                    .finish(error.trailers)
                    .await
            }
        }
    }
}

fn request_timeout(headers: &[embedded_http2::HeaderField]) -> Result<Option<Duration>, Status> {
    let mut values = headers.iter().filter(|field| field.name == "grpc-timeout");
    let Some(field) = values.next() else {
        return Ok(None);
    };
    let (unit, digits) = field
        .value
        .as_bytes()
        .split_last()
        .ok_or(Status::InvalidArgument)?;
    if values.next().is_some() || digits.is_empty() || digits.len() > 8 {
        return Err(Status::InvalidArgument);
    }
    let mut value = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return Err(Status::InvalidArgument);
        }
        value = value * 10 + u64::from(digit - b'0');
    }
    Ok(Some(match unit {
        b'H' => Duration::from_secs(value * 3600),
        b'M' => Duration::from_secs(value * 60),
        b'S' => Duration::from_secs(value),
        b'm' => Duration::from_millis(value),
        b'u' => Duration::from_micros(value),
        b'n' => Duration::from_nanos(value),
        _ => return Err(Status::InvalidArgument),
    }))
}

type DeadlinePoll<'a> = dyn FnMut(&mut Context<'_>) -> Poll<()> + 'a;
type Deadline<'a> = Option<&'a mut DeadlinePoll<'a>>;

async fn before_deadline<T>(
    deadline: &mut Deadline<'_>,
    work: impl Future<Output = T>,
) -> Result<T, Status> {
    let Some(timer) = deadline.as_deref_mut() else {
        return Ok(work.await);
    };
    let mut work = pin!(work);
    poll_fn(|cx| {
        if timer(cx).is_ready() {
            Poll::Ready(Err(Status::DeadlineExceeded))
        } else {
            work.as_mut().poll(cx).map(Ok)
        }
    })
    .await
}

pub struct StreamingCall<'a> {
    body: server::RecvStream,
    respond: server::SendResponse,
    deadline: Deadline<'a>,
}

impl<'a> StreamingCall<'a> {
    /// Split a streaming request from its response producer.
    pub fn streaming<M, const MAX: usize>(
        self,
    ) -> (Request<Streaming<M, MAX>>, ServerStreaming<'a>) {
        let Self {
            body,
            respond,
            deadline,
        } = self;
        let request = Request::new(Streaming {
            body,
            chunk: None,
            offset: 0,
            decoder: MessageDecoder::default(),
            ended: false,
        });
        (request, ServerStreaming { respond, deadline })
    }

    /// Receive exactly one request message, then expose the response stream.
    pub async fn unary<M: MessageDecode + Default, const MAX: usize>(
        self,
    ) -> Result<Option<(M, ServerStreaming<'a>)>, StreamError> {
        let Self {
            mut body,
            respond,
            mut deadline,
        } = self;
        let mut decoder = MessageDecoder::<M, MAX>::default();
        let mut message = None;
        let mut status = Ok(());
        loop {
            let chunk = match before_deadline(&mut deadline, body.data()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    status = Err(error);
                    break;
                }
            };
            let mut bytes = chunk.as_bytes();
            if status.is_err() || bytes.is_empty() {
                continue;
            }
            if message.is_some() {
                status = Err(Status::InvalidArgument);
                continue;
            }
            match decoder.push(&mut bytes) {
                Ok(decoded) if bytes.is_empty() => message = decoded,
                Ok(_) => status = Err(Status::InvalidArgument),
                Err(error) => status = Err(error),
            }
        }

        drop(body);
        let response = ServerStreaming { respond, deadline };
        match status
            .and_then(|()| decoder.finish())
            .and(message.ok_or(Status::InvalidArgument))
        {
            Ok(message) => Ok(Some((message, response))),
            Err(status) => {
                response.reject(status).await?;
                Ok(None)
            }
        }
    }

    pub async fn reject(self, status: Status) -> Result<(), StreamError> {
        let Self { body, respond, .. } = self;
        drop(body);
        ServerStreaming {
            respond,
            deadline: None,
        }
        .reject(status)
        .await
    }
}

/// A typed gRPC request.
pub struct Request<T>(T);

impl<T> Request<T> {
    pub fn new(message: T) -> Self {
        Self(message)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

/// A typed gRPC response.
pub struct Response<T>(T);

impl<T> Response<T> {
    pub fn new(message: T) -> Self {
        Self(message)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

/// A stream of incoming gRPC messages with a bounded decoder.
pub struct Streaming<M, const MAX: usize> {
    body: server::RecvStream,
    chunk: Option<server::Chunk>,
    offset: usize,
    decoder: MessageDecoder<M, MAX>,
    ended: bool,
}

impl<M: MessageDecode + Default, const MAX: usize> Streaming<M, MAX> {
    /// Read one message, retaining receive credit for any coalesced messages.
    pub async fn message(&mut self) -> Result<Option<M>, Status> {
        poll_fn(|cx| Pin::new(&mut *self).poll_next(cx))
            .await
            .transpose()
    }

    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<M>, Status>> {
        loop {
            if let Some(chunk) = &self.chunk {
                let bytes = chunk.as_bytes();
                let mut remaining = &bytes[self.offset..];
                let decoded = self.decoder.push(&mut remaining)?;
                self.offset = bytes.len() - remaining.len();
                if remaining.is_empty() {
                    self.chunk = None;
                }
                if decoded.is_some() {
                    return Poll::Ready(Ok(decoded));
                }
            }
            self.chunk =
                core::task::ready!(self.body.poll_data(cx)).map_err(|_| Status::Unavailable)?;
            self.offset = 0;
            if self.chunk.is_none() {
                return Poll::Ready(self.decoder.finish().map(|()| None));
            }
        }
    }
}

impl<M: MessageDecode + Default, const MAX: usize> Stream for Streaming<M, MAX> {
    type Item = Result<M, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let result = core::task::ready!(self.poll_message(cx));
        self.ended = !matches!(result, Ok(Some(_)));
        Poll::Ready(result.transpose())
    }
}

async fn write_message(
    send: &mut server::SendStream,
    message: &UnaryResponse,
) -> Result<(), StreamError> {
    let mut remaining = message.0.as_slice();
    while !remaining.is_empty() {
        let sent = send.send_data(remaining).await?;
        remaining = &remaining[sent..];
    }
    Ok(())
}

pub struct ServerStreaming<'a> {
    respond: server::SendResponse,
    deadline: Deadline<'a>,
}

impl ServerStreaming<'_> {
    pub async fn start<T>(&mut self, setup: impl Future<Output = T>) -> Result<Option<T>, Status> {
        before_deadline(
            &mut self.deadline,
            until_cancelled(self.respond.closed(), setup),
        )
        .await
    }

    pub async fn reject(self, status: Status) -> Result<(), StreamError> {
        self.respond
            .send_response(response_headers())?
            .finish(vec![field("grpc-status", status.wire_code())])
            .await
    }

    pub async fn reply<M: MessageEncode, const MAX: usize>(
        self,
        message: M,
    ) -> Result<(), StreamError> {
        let mut message = Some(message);
        self.stream::<M, MAX>(OneMessage(&mut message)).await
    }

    /// Poll a typed producer only after the previous message has flushed.
    /// Reset or disconnect drops the producer and its per-call state.
    pub async fn stream<M: MessageEncode, const MAX: usize>(
        mut self,
        items: impl Stream<Item = Result<M, Status>>,
    ) -> Result<(), StreamError> {
        let mut send = self.respond.send_response(response_headers())?;
        let work = async {
            let mut items = pin!(items);
            loop {
                send.flush().await?;
                match until_cancelled(send.closed(), poll_fn(|cx| items.as_mut().poll_next(cx)))
                    .await
                {
                    None => return Ok(None),
                    Some(None) => return Ok(Some(GRPC_OK)),
                    Some(Some(Err(status))) => return Ok(Some(status.wire_code())),
                    Some(Some(Ok(message))) => {
                        let response = match UnaryResponse::encode::<_, MAX>(message) {
                            Ok(response) => response,
                            Err(status) => return Ok(Some(status.wire_code())),
                        };
                        write_message(&mut send, &response).await?;
                    }
                }
            }
        };
        let code = match before_deadline(&mut self.deadline, work).await {
            Ok(Ok(Some(code))) => code,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => return Err(error),
            Err(status) => status.wire_code(),
        };
        finish(send, code).await
    }
}

async fn finish(send: server::SendStream, code: &'static str) -> Result<(), StreamError> {
    let trailers = vec![field("grpc-status", code)];
    if code == Status::DeadlineExceeded.wire_code() {
        send.finish_discarding_data(trailers).await
    } else {
        send.finish(trailers).await
    }
}

struct OneMessage<'a, M>(&'a mut Option<M>);

impl<M> Stream for OneMessage<'_, M> {
    type Item = Result<M, Status>;

    fn poll_next(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Self::Item>> {
        core::task::Poll::Ready(self.0.take().map(Ok))
    }
}

fn response_headers() -> alloc::vec::Vec<embedded_http2::HeaderField> {
    vec![
        field(":status", HTTP_OK),
        field("content-type", CONTENT_TYPE),
    ]
}

// Poll cancellation first so a reset wins when both futures are ready.
async fn until_cancelled<T>(
    cancelled: impl Future<Output = ()>,
    work: impl Future<Output = T>,
) -> Option<T> {
    let mut cancelled = pin!(cancelled);
    let mut work = pin!(work);
    poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            work.as_mut().poll(cx).map(Some)
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grpc_timeout_units_and_rejects_invalid_values() {
        for (value, expected) in [
            ("1H", Duration::from_secs(3600)),
            ("2M", Duration::from_secs(120)),
            ("3S", Duration::from_secs(3)),
            ("4m", Duration::from_millis(4)),
            ("5u", Duration::from_micros(5)),
            ("6n", Duration::from_nanos(6)),
            ("0S", Duration::ZERO),
        ] {
            assert_eq!(
                request_timeout(&[field("grpc-timeout", value)]),
                Ok(Some(expected))
            );
        }
        for invalid in ["", "m", "1s", "-1S", "123456789n", "1 S"] {
            assert_eq!(
                request_timeout(&[field("grpc-timeout", invalid)]),
                Err(Status::InvalidArgument)
            );
        }
        assert_eq!(
            request_timeout(&[field("grpc-timeout", "1S"), field("grpc-timeout", "2S")]),
            Err(Status::InvalidArgument)
        );
        assert_eq!(request_timeout(&[]), Ok(None));
    }
}
