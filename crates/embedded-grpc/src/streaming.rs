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
    /// Run a typed bidirectional handler until completion or peer reset.
    pub async fn bidirectional<M, R, const MAX: usize>(
        self,
        handler: impl AsyncFnOnce(&mut Bidirectional<M, R, MAX>) -> Result<(), Status>,
    ) -> Result<(), StreamError>
    where
        M: MessageDecode + Default,
        R: MessageEncode,
    {
        let Self { body, respond } = self;
        let mut call = Bidirectional {
            body,
            send: respond.send_response(response_headers())?,
            chunk: None,
            offset: 0,
            decoder: MessageDecoder::default(),
            reply: core::marker::PhantomData,
        };
        let cancel = call.send.cancellation();
        let code = match select(cancel, handler(&mut call)).await {
            Either::First(()) => return Ok(()),
            Either::Second(Ok(())) => GRPC_OK,
            Either::Second(Err(status)) => status.wire_code(),
        };
        // The handler may finish before the peer half-closes. Dropping the
        // receive handle discards unread data while the driver flushes trailers.
        drop(call.chunk);
        drop(call.body);
        call.send.finish(vec![field("grpc-status", code)]).await
    }

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

/// One bidirectional RPC with bounded inbound framing and flushed replies.
pub struct Bidirectional<M, R, const MAX: usize> {
    body: server::RecvStream,
    send: server::SendStream,
    chunk: Option<server::Chunk>,
    offset: usize,
    decoder: MessageDecoder<M, MAX>,
    reply: core::marker::PhantomData<fn(R)>,
}

impl<M: MessageDecode + Default, R: MessageEncode, const MAX: usize> Bidirectional<M, R, MAX> {
    /// Read one message, retaining receive credit for any coalesced messages.
    pub async fn message(&mut self) -> Result<Option<M>, Status> {
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
                    return Ok(decoded);
                }
            }
            self.chunk = self.body.data().await.map_err(|_| Status::Unavailable)?;
            self.offset = 0;
            if self.chunk.is_none() {
                self.decoder.finish()?;
                return Ok(None);
            }
        }
    }

    /// Send and flush one reply before handling another request message.
    pub async fn send(&mut self, message: R) -> Result<(), Status> {
        let encoded = UnaryResponse::encode::<_, MAX>(message)?;
        write_message(&mut self.send, &encoded)
            .await
            .map_err(|_| Status::Unavailable)?;
        self.send.flush().await.map_err(|_| Status::Unavailable)
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
                    write_message(&mut send, &response).await?;
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
