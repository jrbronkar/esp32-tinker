//! Blocking gRPC host client shared by CLI presentation and future GUI callers.
pub mod discovery;
mod grpc_codec;
pub mod upload;
use grpc_codec::RawCodec;
use ota_protocol::{Request, RequestPayload, Response, ResponsePayload, StatusResponse, VERSION};
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
    let endpoint =
        Endpoint::from_shared(format!("http://{address}")).map_err(|e| invalid(e.to_string()))?;
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
    address: SocketAddr,
    end: tokio::time::Instant,
    request: Request,
    path: &'static str,
    timings: &mut QueryTimings,
) -> io::Result<Response> {
    let mut client = connect(address, end, timings).await?;
    client.ready().await.map_err(|e| invalid(e.to_string()))?;
    let started = Instant::now();
    let bytes = ota_protocol::encode(&request)
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
    let started = Instant::now();
    let response: Response = ota_protocol::decode(&reply.into_inner())
        .map_err(|e| invalid(format!("decode response: {e:?}")))?;
    if response.protocol_version != VERSION {
        return Err(invalid("unsupported response version"));
    }
    if response.request_id != request.request_id {
        return Err(invalid("response request ID mismatch"));
    }
    if let Some(ResponsePayload::Error(error)) = &response.payload {
        return Err(invalid(format!(
            "device error: {}",
            error.message.escape_debug()
        )));
    }
    timings.decode_validate = started.elapsed();
    Ok(response)
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
        let request = Request {
            protocol_version: VERSION,
            request_id: 1,
            payload: Some(RequestPayload::Status(ota_protocol::StatusRequest {
                include_timings: device_timings,
                include_boot,
            })),
        };
        let response = query(
            address,
            end,
            request,
            device_grpc::STATUS_PATH,
            &mut timings,
        )
        .await?;
        let validated = Instant::now();
        let Some(ResponsePayload::Status(status)) = response.payload else {
            return Err(invalid("response has no status payload"));
        };
        if status.device_id.is_empty()
            || status.application.is_empty()
            || status.firmware_version.is_empty()
            || status.chip.is_empty()
        {
            return Err(invalid("status response is missing identity fields"));
        }
        timings.decode_validate += validated.elapsed();
        timings.total = started.elapsed();
        Ok(StatusQuery { status, timings })
    })
}
/// Read one retained log batch over the same gRPC management endpoint.
pub fn read_logs(
    address: SocketAddr,
    timeout: Duration,
    cursor: ota_protocol::ReadLogsRequest,
) -> io::Result<ota_protocol::ReadLogsResponse> {
    let end = deadline(Instant::now(), timeout)?;
    runtime()?.block_on(async {
        let request = Request {
            protocol_version: VERSION,
            request_id: 1,
            payload: Some(RequestPayload::ReadLogs(cursor.clone())),
        };
        let response = query(
            address,
            end,
            request,
            device_grpc::READ_LOGS_PATH,
            &mut QueryTimings::default(),
        )
        .await?;
        let Some(ResponsePayload::Logs(logs)) = response.payload else {
            return Err(invalid("missing log payload"));
        };
        validate_logs(&cursor, &logs)?;
        Ok(logs)
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
