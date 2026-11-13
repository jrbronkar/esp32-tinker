use alloc::vec;
use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};
use embedded_http2::server::{self, StreamError};
use embedded_io_async::{Read, Write};
use futures_core::Stream;
use micropb::{MessageDecode, MessageEncode};

use crate::{
    CONTENT_TYPE, ConnectionError, GRPC_OK, HTTP_OK, MessageDecoder, Status, UnaryResponse, field,
    rpc_path,
};

/// A service that can handle multiple calls on the same local executor.
#[allow(async_fn_in_trait)]
pub trait StreamingService {
    async fn call(&self, path: &str, call: StreamingCall) -> Result<(), StreamError>;
}

pub async fn serve_streaming<Io: Read + Write, S: StreamingService>(
    io: Io,
    service: &S,
) -> Result<(), ConnectionError<Io::Error>> {
    embedded_http2::service::serve_streams(io, |request| async move {
        // A reset or failed response ends its own RPC. The driver reports
        // connection errors independently.
        let _ = dispatch(service, request).await;
        Ok(())
    })
    .await
    .map_err(ConnectionError)
}

async fn dispatch<S: StreamingService>(
    service: &S,
    request: server::Request,
) -> Result<(), StreamError> {
    let server::Request {
        head,
        mut body,
        respond,
    } = request;
    match rpc_path(&head.headers) {
        Ok(path) => service.call(path, StreamingCall { body, respond }).await,
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

pub struct StreamingCall {
    body: server::RecvStream,
    respond: server::SendResponse,
}

impl StreamingCall {
    /// Receive exactly one request message, then expose the response stream.
    pub async fn unary<M: MessageDecode + Default, const MAX: usize>(
        self,
    ) -> Result<Option<(M, ServerStreaming)>, StreamError> {
        let Self { mut body, respond } = self;
        let mut decoder = MessageDecoder::<M, MAX>::default();
        let mut message = None;
        let mut status = Ok(());
        while let Some(chunk) = body.data().await? {
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

        let response = ServerStreaming { respond };
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
        let Self { mut body, respond } = self;
        while body.data().await?.is_some() {}
        ServerStreaming { respond }.reject(status).await
    }
}

pub struct ServerStreaming {
    respond: server::SendResponse,
}

impl ServerStreaming {
    pub async fn start<T>(&self, setup: impl core::future::Future<Output = T>) -> Option<T> {
        until_cancelled(self.respond.closed(), setup).await
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
        self,
        items: impl Stream<Item = Result<M, Status>>,
    ) -> Result<(), StreamError> {
        let mut items = pin!(items);
        let mut send = self.respond.send_response(response_headers())?;
        let code = loop {
            send.flush().await?;
            match until_cancelled(send.closed(), poll_fn(|cx| items.as_mut().poll_next(cx))).await {
                None => return Ok(()),
                Some(None) => break GRPC_OK,
                Some(Some(Err(status))) => break status.wire_code(),
                Some(Some(Ok(message))) => {
                    let response = match UnaryResponse::encode::<_, MAX>(message) {
                        Ok(response) => response,
                        Err(status) => break status.wire_code(),
                    };
                    let mut remaining = response.0.as_slice();
                    while !remaining.is_empty() {
                        let sent = send.send_data(remaining).await?;
                        remaining = &remaining[sent..];
                    }
                }
            }
        };
        send.finish(vec![field("grpc-status", code)]).await
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
