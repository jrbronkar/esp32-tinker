//! Blocking gRPC host client shared by CLI presentation and future GUI callers.
pub mod discovery;
mod grpc_codec;
pub mod tui;
pub mod upload;
use grpc_codec::RawCodec;
use ota_protocol::StatusResponse;
use std::{
    io,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tonic::{
    client::Grpc,
    transport::{Channel, Endpoint},
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn rpc(error: tonic::Status) -> io::Error {
    let kind = match error.code() {
        tonic::Code::DeadlineExceeded => io::ErrorKind::TimedOut,
        tonic::Code::Unavailable | tonic::Code::Unknown => io::ErrorKind::ConnectionAborted,
        tonic::Code::Cancelled => io::ErrorKind::Interrupted,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(
        kind,
        format!("gRPC {}: {}", error.code(), error.message().escape_debug()),
    )
}
/// Host elapsed phases. Connect includes TCP retry wait and channel setup. RPC
/// includes HTTP/2 setup, request transmission, server work, body and trailers.
#[derive(Debug, Clone, Default)]
pub struct QueryTimings {
    pub total: Duration,
    pub connect: Duration,
    pub retry_wait: Duration,
    pub encode: Duration,
    pub rpc: Duration,
    pub decode_validate: Duration,
    pub refused_connects: u32,
}
#[derive(Debug)]
pub struct StatusQuery {
    pub status: StatusResponse,
    pub timings: QueryTimings,
}
fn deadline(started: Instant, timeout: Duration) -> io::Result<tokio::time::Instant> {
    if timeout.is_zero() {
        return Err(invalid("timeout must be positive"));
    }
    started
        .checked_add(timeout)
        .map(tokio::time::Instant::from_std)
        .ok_or_else(|| invalid("timeout too large"))
}
fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "management query deadline exceeded",
    )
}
fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}
async fn connect(
    address: SocketAddr,
    end: tokio::time::Instant,
    timings: &mut QueryTimings,
    keep_alive: bool,
) -> io::Result<Grpc<Channel>> {
    let started = Instant::now();
    let stream = loop {
        match tokio::time::timeout_at(end, tokio::net::TcpStream::connect(address))
            .await
            .map_err(|_| timed_out())?
        {
            Ok(stream) => break stream,
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                timings.refused_connects += 1;
                let retry = Instant::now();
                tokio::time::timeout_at(end, tokio::time::sleep(Duration::from_millis(50)))
                    .await
                    .map_err(|_| timed_out())?;
                timings.retry_wait += retry.elapsed();
            }
            Err(error) => return Err(error),
        }
    };
    stream.set_nodelay(true)?;
    // One established TCP connection only. The channel must not silently dial
    // again/replay an established query after a transport failure.
    let mut stream = Some(stream);
    let connector = tower::service_fn(move |_| {
        std::future::ready(
            stream
                .take()
                .map(hyper_util::rt::TokioIo::new)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "query connection cannot be replayed",
                    )
                }),
        )
    });
    let mut endpoint =
        Endpoint::from_shared(format!("http://{address}")).map_err(|e| invalid(e.to_string()))?;
    if keep_alive {
        endpoint = endpoint
            .http2_keep_alive_interval(Duration::from_secs(5))
            .keep_alive_while_idle(true);
    }
    let channel = tokio::time::timeout_at(end, endpoint.connect_with_connector(connector))
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("HTTP/2 setup: {e}"),
            )
        })?;
    timings.connect = started.elapsed();
    Ok(Grpc::new(channel)
        .max_decoding_message_size(ota_protocol::MAX_FRAME)
        .max_encoding_message_size(ota_protocol::MAX_FRAME))
}
async fn query(
    client: &mut Grpc<Channel>,
    end: tokio::time::Instant,
    request: &impl ota_protocol::MessageEncode,
    path: &'static str,
    timings: &mut QueryTimings,
) -> io::Result<Vec<u8>> {
    tokio::time::timeout_at(end, client.ready())
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| invalid(e.to_string()))?;
    let started = Instant::now();
    let bytes = ota_protocol::encode(request)
        .map_err(|e| invalid(format!("encode query: {e:?}")))?
        .to_vec();
    timings.encode = started.elapsed();
    let started = Instant::now();
    let mut message = tonic::Request::new(bytes);
    message.set_timeout(end.saturating_duration_since(tokio::time::Instant::now()));
    let reply = tokio::time::timeout_at(
        end,
        client.unary(
            message,
            path.parse().map_err(|_| invalid("invalid RPC path"))?,
            RawCodec,
        ),
    )
    .await
    .map_err(|_| timed_out())?
    .map_err(rpc)?;
    timings.rpc = started.elapsed();
    Ok(reply.into_inner())
}

pub fn query_status(address: SocketAddr, timeout: Duration) -> io::Result<StatusResponse> {
    query_status_with_options(address, timeout, false, false).map(|q| q.status)
}
pub fn query_status_profiled(address: SocketAddr, timeout: Duration) -> io::Result<StatusQuery> {
    query_status_with_options(address, timeout, true, false)
}
pub fn query_status_with_diagnostics(
    address: SocketAddr,
    timeout: Duration,
    device_timings: bool,
) -> io::Result<StatusQuery> {
    query_status_with_options(address, timeout, device_timings, false)
}
pub fn query_status_with_options(
    address: SocketAddr,
    timeout: Duration,
    device_timings: bool,
    include_boot: bool,
) -> io::Result<StatusQuery> {
    let started = Instant::now();
    let end = deadline(started, timeout)?;
    runtime()?.block_on(async {
        let mut timings = QueryTimings::default();
        let request = ota_protocol::StatusRequest {
            include_timings: device_timings,
            include_boot,
        };
        let mut client = connect(address, end, &mut timings, false).await?;
        let response = query(
            &mut client,
            end,
            &request,
            device_grpc::STATUS_PATH,
            &mut timings,
        )
        .await?;
        let validated = Instant::now();
        let status = decode_status(&response)?;
        timings.decode_validate += validated.elapsed();
        timings.total = started.elapsed();
        Ok(StatusQuery { status, timings })
    })
}
fn decode_status(response: &[u8]) -> io::Result<StatusResponse> {
    let status: StatusResponse =
        ota_protocol::decode(response).map_err(|_| invalid("invalid status response"))?;
    if status.device_id.is_empty()
        || status.application.is_empty()
        || status.firmware_version.is_empty()
        || status.chip.is_empty()
    {
        return Err(invalid("status response is missing identity fields"));
    }
    Ok(status)
}

async fn open_logs(
    client: &mut Grpc<Channel>,
    end: tokio::time::Instant,
    cursor: &ota_protocol::ReadLogsRequest,
) -> io::Result<tonic::Streaming<Vec<u8>>> {
    tokio::time::timeout_at(end, async {
        client
            .ready()
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        let bytes = ota_protocol::encode(cursor).map_err(|_| invalid("encode log cursor"))?;
        client
            .server_streaming(
                tonic::Request::new(bytes.to_vec()),
                device_grpc::LOGS_PATH
                    .parse()
                    .map_err(|_| invalid("invalid logs RPC path"))?,
                RawCodec,
            )
            .await
            .map(|response| response.into_inner())
            .map_err(rpc)
    })
    .await
    .map_err(|_| timed_out())?
}

fn decode_logs(
    cursor: &mut ota_protocol::ReadLogsRequest,
    bytes: &[u8],
) -> io::Result<ota_protocol::ReadLogsResponse> {
    let batch: ota_protocol::ReadLogsResponse =
        ota_protocol::decode(bytes).map_err(|_| invalid("invalid log batch"))?;
    validate_logs(cursor, &batch)?;
    cursor.boot_id = batch.boot_id;
    cursor.after_sequence = batch.next_sequence;
    Ok(batch)
}

/// Read retained logs on one stream, optionally waiting for new entries.
pub fn stream_logs(
    address: SocketAddr,
    timeout: Duration,
    follow: bool,
    cursor: &mut ota_protocol::ReadLogsRequest,
    mut on_batch: impl FnMut(&ota_protocol::ReadLogsResponse),
) -> io::Result<()> {
    let end = deadline(Instant::now(), timeout)?;
    runtime()?.block_on(async {
        let mut client = connect(address, end, &mut QueryTimings::default(), follow).await?;
        let mut stream = open_logs(&mut client, end, cursor).await?;
        let mut started = false;
        loop {
            let message = if follow && started {
                stream.message().await
            } else {
                tokio::time::timeout_at(end, stream.message())
                    .await
                    .map_err(|_| timed_out())?
            }
            .map_err(rpc)?;
            let Some(bytes) = message else {
                return if follow && started {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "log stream ended before the read completed",
                    ))
                };
            };
            let batch = decode_logs(cursor, &bytes)?;
            on_batch(&batch);
            if !follow && !batch.more {
                return Ok(());
            }
            started = true;
        }
    })
}
pub(crate) fn validate_logs(
    cursor: &ota_protocol::ReadLogsRequest,
    logs: &ota_protocol::ReadLogsResponse,
) -> io::Result<()> {
    if logs.boot_id == 0 {
        return Err(invalid("missing log boot ID"));
    }
    if cursor.boot_id != 0 && logs.boot_id != cursor.boot_id && !logs.reset {
        return Err(invalid("changed log boot ID without cursor reset"));
    }
    let mut previous = if logs.reset { 0 } else { cursor.after_sequence };
    for entry in &logs.entries {
        if entry.sequence <= previous {
            return Err(invalid("log entries are not strictly increasing"));
        }
        previous = entry.sequence;
    }
    if logs.next_sequence != previous {
        return Err(invalid("log cursor does not match returned entries"));
    }
    Ok(())
}
