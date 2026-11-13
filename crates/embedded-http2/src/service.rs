//! Buffered HTTP exchanges over a multiplexed HTTP/2 connection.

use alloc::{boxed::Box, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::Poll,
};
use embedded_io_async::{Read, Write};

use crate::{
    Config, Error, HeaderField, MAX_BODY,
    server::{self, StreamError},
};

pub struct Request {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

pub struct Response {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

/// Handles one complete request at a time. The caller supplies the executor.
#[allow(async_fn_in_trait)]
pub trait Service {
    async fn call(&mut self, request: Request) -> Response;
}

/// Serve buffered exchanges while the connection driver advances the socket.
/// The socket's reads must be cancellation-safe. Request handlers run one at a
/// time, while body reads and response writes can progress on either stream.
pub async fn serve<Io: Read + Write, S: Service>(
    io: Io,
    service: &mut S,
) -> Result<(), Error<Io::Error>> {
    serve_with_config(io, service, Config::default()).await
}

/// Serve buffered exchanges with the supplied per-connection stream limit.
pub async fn serve_with_config<Io: Read + Write, S: Service>(
    io: Io,
    service: &mut S,
    config: Config,
) -> Result<(), Error<Io::Error>> {
    let service = ServiceSlot {
        value: RefCell::new(Some(service)),
        released: Cell::new(false),
    };
    drive_streams(
        io,
        config,
        |request| async { exchange(&service, request).await },
        || service.released.replace(false),
    )
    .await
}

/// Drive the connection alongside at most one handler per active stream.
pub async fn serve_streams<Io, Task>(
    io: Io,
    handle: impl Fn(server::Request) -> Task,
) -> Result<(), Error<Io::Error>>
where
    Io: Read + Write,
    Task: Future<Output = Result<(), Error<Io::Error>>>,
{
    serve_streams_with_config(io, Config::default(), handle).await
}

/// Drive streaming handlers with the supplied per-connection stream limit.
pub async fn serve_streams_with_config<Io, Task>(
    io: Io,
    config: Config,
    handle: impl Fn(server::Request) -> Task,
) -> Result<(), Error<Io::Error>>
where
    Io: Read + Write,
    Task: Future<Output = Result<(), Error<Io::Error>>>,
{
    drive_streams(io, config, handle, || false).await
}

async fn drive_streams<Io, Task>(
    io: Io,
    config: Config,
    handle: impl Fn(server::Request) -> Task,
    handlers_need_poll: impl Fn() -> bool,
) -> Result<(), Error<Io::Error>>
where
    Io: Read + Write,
    Task: Future<Output = Result<(), Error<Io::Error>>>,
{
    let (connection, incoming) = server::handshake_with_config(io, config).await?;
    let mut driver = pin!(connection.run());
    let mut handlers = pin!(serve_requests(
        incoming,
        handle,
        handlers_need_poll,
        config.max_concurrent_streams.get()
    ));
    let mut transport_result = None;
    let mut handler_result = None;
    poll_fn(|cx| {
        if transport_result.is_none()
            && let Poll::Ready(result) = driver.as_mut().poll(cx)
        {
            transport_result = Some(result);
        }
        if handler_result.is_none()
            && let Poll::Ready(result) = handlers.as_mut().poll(cx)
        {
            handler_result = Some(result);
        }
        match (&mut transport_result, &mut handler_result) {
            (Some(transport), Some(handlers)) => Poll::Ready(
                core::mem::replace(handlers, Ok(())).and(core::mem::replace(transport, Ok(()))),
            ),
            _ => Poll::Pending,
        }
    })
    .await
}

async fn serve_requests<E, Task: Future<Output = Result<(), Error<E>>>>(
    mut incoming: server::Incoming,
    handle: impl Fn(server::Request) -> Task,
    handlers_need_poll: impl Fn() -> bool,
    stream_limit: u32,
) -> Result<(), Error<E>> {
    let mut exchanges: Vec<Pin<Box<Task>>> = Vec::new();
    poll_fn(|cx| {
        let mut index = 0;
        while let Some(task) = exchanges.get_mut(index) {
            match task.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    drop(exchanges.swap_remove(index));
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => index += 1,
            }
        }
        if (exchanges.len() as u64) < u64::from(stream_limit) {
            match incoming.poll_accept(cx) {
                Poll::Ready(Some(request)) => {
                    exchanges.push(Box::pin(handle(request)));
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => {}
            }
        }
        if handlers_need_poll() {
            // A slot visited earlier may be waiting for the service.
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

struct ServiceSlot<'a, S> {
    value: RefCell<Option<&'a mut S>>,
    released: Cell<bool>,
}

struct ServiceLease<'a, 's, S> {
    service: &'a ServiceSlot<'s, S>,
    value: Option<&'s mut S>,
}

impl<S> Drop for ServiceLease<'_, '_, S> {
    fn drop(&mut self) {
        self.service.value.replace(self.value.take());
        self.service.released.set(true);
    }
}

async fn exchange<E, S: Service>(
    service: &ServiceSlot<'_, S>,
    request: server::Request,
) -> Result<(), Error<E>> {
    match exchange_stream(service, request).await {
        Ok(()) | Err(ExchangeError::Stream) => Ok(()),
        Err(ExchangeError::BodyLimit) => Err(Error::BodyLimit),
    }
}

enum ExchangeError {
    Stream,
    BodyLimit,
}

impl From<StreamError> for ExchangeError {
    fn from(_: StreamError) -> Self {
        Self::Stream
    }
}

async fn exchange_stream<S: Service>(
    service: &ServiceSlot<'_, S>,
    server::Request {
        head,
        mut body,
        respond,
    }: server::Request,
) -> Result<(), ExchangeError> {
    let mut request = Request {
        headers: head.headers,
        body: Vec::new(),
        trailers: Vec::new(),
    };
    while let Some(chunk) = body.data().await? {
        let bytes = chunk.as_bytes();
        if request.body.len() + bytes.len() > MAX_BODY {
            // Dropping the unused response handle resets this stream.
            return Ok(());
        }
        request.body.extend_from_slice(bytes);
    }
    request.trailers = body.trailers().unwrap_or_default();
    let response = {
        let mut closed = pin!(respond.closed());
        let mut call = pin!(async {
            let mut lease = poll_fn(|_| match service.value.borrow_mut().take() {
                Some(value) => Poll::Ready(ServiceLease {
                    service,
                    value: Some(value),
                }),
                None => Poll::Pending,
            })
            .await;
            match lease.value.as_deref_mut() {
                Some(service) => Some(service.call(request).await),
                None => None,
            }
        });
        poll_fn(|cx| {
            if closed.as_mut().poll(cx).is_ready() {
                Poll::Ready(None)
            } else {
                call.as_mut().poll(cx)
            }
        })
        .await
    };
    let Some(response) = response else {
        return Ok(());
    };
    if response.body.len() > MAX_BODY {
        return Err(ExchangeError::BodyLimit);
    }

    if response.body.is_empty() && response.trailers.is_empty() {
        return Ok(respond.finish_headers(response.headers).await?);
    }
    let mut send = respond.send_response(response.headers)?;
    let mut remaining = response.body.as_slice();
    while !remaining.is_empty() {
        let sent = send.send_data(remaining).await?;
        remaining = &remaining[sent..];
    }
    Ok(send.finish(response.trailers).await?)
}
