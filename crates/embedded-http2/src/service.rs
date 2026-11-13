//! Buffered HTTP exchanges over a multiplexed HTTP/2 connection.

use alloc::vec::Vec;
use embassy_futures::{
    join::join,
    select::{Either, select},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embedded_io_async::{Read, Write};
use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::{
    Error, HeaderField, MAX_BODY, MAX_STREAMS,
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
    let (connection, incoming) = server::handshake(io).await?;
    let (transport, handlers) = join(connection.run(), serve_requests(incoming, service)).await;
    handlers.and(transport)
}

async fn serve_requests<E, S: Service>(
    mut incoming: server::Incoming,
    service: &mut S,
) -> Result<(), Error<E>> {
    let service = Mutex::<NoopRawMutex, _>::new(service);
    let mut exchanges = FuturesUnordered::new();
    loop {
        if exchanges.len() == MAX_STREAMS {
            if let Some(result) = exchanges.next().await {
                result?;
            }
        } else if exchanges.is_empty() {
            let Some(request) = incoming.accept().await else {
                return Ok(());
            };
            exchanges.push(exchange(&service, request));
        } else {
            match select(exchanges.next(), incoming.accept()).await {
                Either::First(Some(result)) => result?,
                Either::First(None) => {}
                Either::Second(Some(request)) => exchanges.push(exchange(&service, request)),
                Either::Second(None) => return Ok(()),
            }
        }
    }
}

async fn exchange<E, S: Service>(
    service: &Mutex<NoopRawMutex, &mut S>,
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
    service: &Mutex<NoopRawMutex, &mut S>,
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
    let call = async { service.lock().await.call(request).await };
    let response = match select(respond.closed(), call).await {
        Either::First(()) => return Ok(()),
        Either::Second(response) => response,
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
