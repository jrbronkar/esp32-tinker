//! Blocking host client, independent of CLI presentation and reusable by a future GUI.
pub mod discovery;
use ota_protocol::{Request, Response, ResponsePayload, StatusResponse, MAX_FRAME, VERSION};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Host-observed phases. Connect includes retry_wait; phase durations are not
/// device CPU timings or pure network RTT. All timestamps use a monotonic clock.
#[derive(Debug, Clone, Default)]
pub struct QueryTimings {
    pub total: Duration,
    pub connect: Duration,
    pub retry_wait: Duration,
    pub write: Duration,
    pub first_byte: Duration,
    pub read: Duration,
    pub decode_validate: Duration,
    pub refused_connects: u32,
}

#[derive(Debug)]
pub struct StatusQuery {
    pub status: StatusResponse,
    pub timings: QueryTimings,
}

/// One request/response per connection, with one total deadline (including connect).
pub fn query_status(address: SocketAddr, timeout: Duration) -> io::Result<StatusResponse> {
    query_status_with_options(address, timeout, false, false).map(|query| query.status)
}

pub fn query_status_profiled(address: SocketAddr, timeout: Duration) -> io::Result<StatusQuery> {
    query_status_with_options(address, timeout, true, false)
}

/// Collect host timings, optionally requesting device timing/serial tracing.
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
    let mut timings = QueryTimings::default();
    if timeout.is_zero() {
        return Err(invalid("timeout must be positive"));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid("timeout too large"))?;
    // A single-socket device can briefly stop listening while closing the last
    // session. Retry only refused connects; never replay an established request.
    let mut stream = loop {
        match TcpStream::connect_timeout(&address, remaining(deadline)?) {
            Ok(stream) => break stream,
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                timings.refused_connects += 1;
                let sleep_started = Instant::now();
                std::thread::sleep(Duration::from_millis(50).min(remaining(deadline)?));
                timings.retry_wait += sleep_started.elapsed();
            }
            Err(error) => return Err(error),
        }
    };
    timings.connect = started.elapsed();
    let write_started = Instant::now();
    stream.set_nodelay(true)?;
    let mut request = ota_protocol::status_request(1);
    if let Some(ota_protocol::RequestPayload::Status(status)) = &mut request.payload {
        status.include_timings = device_timings;
        status.include_boot = include_boot;
    }
    write_request(&mut stream, &request, deadline)?;
    timings.write = write_started.elapsed();
    let response_started = Instant::now();
    let mut prefix = [0u8; 4];
    read_exact(&mut stream, &mut prefix[..1], deadline)?;
    timings.first_byte = response_started.elapsed();
    let read_started = Instant::now();
    read_exact(&mut stream, &mut prefix[1..], deadline)?;
    let length = ota_protocol::frame_length(prefix)
        .map_err(|_| invalid("response length outside 1..=512"))?;
    let mut body = [0u8; MAX_FRAME];
    read_exact(&mut stream, &mut body[..length], deadline)?;
    timings.read = read_started.elapsed();
    let decode_started = Instant::now();
    let response: Response = ota_protocol::decode(&body[..length])
        .map_err(|e| invalid(format!("decode response: {e:?}")))?;
    if response.protocol_version != VERSION {
        return Err(invalid(format!(
            "unsupported response version {}",
            response.protocol_version
        )));
    }
    if response.request_id != request.request_id {
        return Err(invalid("response request ID mismatch"));
    }
    match response.payload {
        Some(ResponsePayload::Status(status)) => {
            if status.device_id.is_empty()
                || status.application.is_empty()
                || status.firmware_version.is_empty()
                || status.chip.is_empty()
            {
                return Err(invalid("status response is missing identity fields"));
            }
            timings.decode_validate = decode_started.elapsed();
            timings.total = started.elapsed();
            Ok(StatusQuery { status, timings })
        }
        Some(ResponsePayload::Error(error)) => Err(invalid(format!(
            "device error {:?}: {}",
            error.code,
            error.message.escape_debug()
        ))),
        _ => Err(invalid("response has no supported payload")),
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "status deadline exceeded"))
}

fn write_request(stream: &mut TcpStream, request: &Request, deadline: Instant) -> io::Result<()> {
    let body =
        ota_protocol::encode(request).map_err(|e| invalid(format!("encode request: {e:?}")))?;
    for mut bytes in [&(body.len() as u32).to_be_bytes()[..], body.as_slice()] {
        while !bytes.is_empty() {
            stream.set_write_timeout(Some(remaining(deadline)?))?;
            let n = stream.write(bytes)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n..];
        }
    }
    Ok(())
}

fn read_exact(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let n = stream.read(bytes)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "device disconnected before complete response",
            ));
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}

pub mod upload;
