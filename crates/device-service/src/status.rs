//! Device status service with opt-in unsigned development OTA uploads.
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_println::println;
use ota_protocol::{
    DeviceTimings, Request, RequestPayload, ResponsePayload, StatusResponse, MAX_FRAME, PORT,
    SESSION_TIMEOUT_SECS,
};

#[embassy_executor::task]
pub async fn server(
    stack: Stack<'static>,
    identity: StatusResponse,
    flash: esp_storage::FlashStorage<'static>,
) {
    #[cfg(feature = "dev-ota")]
    let mut identity = identity;
    #[cfg(feature = "dev-ota")]
    let mut target = crate::flash_target::FlashTarget::new(flash);
    #[cfg(feature = "dev-ota")]
    {
        identity.ota_available = target.available;
        let _ = target.confirm_running();
    }
    #[cfg(not(feature = "dev-ota"))]
    let _ = flash;
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(SESSION_TIMEOUT_SECS)));
    println!("Protobuf device service on TCP port {PORT} (unauthenticated)");
    #[cfg(feature = "dev-ota")]
    println!("Development OTA available: {}", identity.ota_available);
    let mut sequence = 0u64;
    loop {
        stack.wait_config_up().await;
        if socket.accept(PORT).await.is_ok() {
            sequence = sequence.wrapping_add(1);
            let mut trace = Trace::default();
            // Absolute session deadline, including reads, writes, and ACKs. A
            // trickle of bytes must not monopolize the sole listener forever.
            match with_timeout(
                Duration::from_secs(if cfg!(feature = "dev-ota") {
                    180
                } else {
                    SESSION_TIMEOUT_SECS
                }),
                session(
                    &mut socket,
                    &identity,
                    &mut trace,
                    #[cfg(feature = "dev-ota")]
                    &mut target,
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(())) => println!("Status session closed: invalid frame or connection error"),
                Err(_) => println!("Status session timed out"),
            }
            // Logging is after response/close so serial output does not delay
            // that response. It can still delay rearming the single listener.
            if trace.enabled {
                println!("STATUS_TRACE seq={} id={} phase={} accepted_us={} read_us={} decode_us={} prepare_us={} encode_us={} enqueue_us={} ack_wait_us={} close_us={}",
                    sequence, trace.request_id, trace.phase, trace.accepted_us, trace.read_us,
                    trace.decode_us, trace.prepare_us, trace.encode_us,
                    trace.enqueue_us, trace.ack_wait_us, trace.close_us);
            }
        }
        socket.abort();
        let _ = with_timeout(Duration::from_millis(250), socket.flush()).await;
        // Also yield/back off after accept errors, e.g. while DHCP is changing.
        Timer::after(Duration::from_millis(10)).await;
    }
}

#[derive(Default)]
struct Trace {
    enabled: bool,
    phase: &'static str,
    request_id: u32,
    accepted_us: u64,
    read_us: u64,
    decode_us: u64,
    prepare_us: u64,
    encode_us: u64,
    enqueue_us: u64,
    ack_wait_us: u64,
    close_us: u64,
}

async fn session(
    socket: &mut TcpSocket<'_>,
    identity: &StatusResponse,
    trace: &mut Trace,
    #[cfg(feature = "dev-ota")] target: &mut crate::flash_target::FlashTarget,
) -> Result<(), ()> {
    trace.phase = "read";
    let accepted = Instant::now();
    trace.accepted_us = accepted.as_micros();
    let mut prefix = [0u8; 4];
    with_timeout(Duration::from_secs(5), read_exact(socket, &mut prefix))
        .await
        .map_err(|_| ())??;
    let length = ota_protocol::frame_length(prefix).map_err(|_| ())?;
    let mut body = [0u8; MAX_FRAME];
    with_timeout(
        Duration::from_secs(5),
        read_exact(socket, &mut body[..length]),
    )
    .await
    .map_err(|_| ())??;
    trace.read_us = accepted.elapsed().as_micros();
    trace.phase = "decode";
    let decoded = Instant::now();
    let request: Request = ota_protocol::decode(&body[..length]).map_err(|_| ())?;
    trace.decode_us = decoded.elapsed().as_micros();
    #[cfg(feature = "dev-ota")]
    if request.protocol_version == ota_protocol::UPLOAD_VERSION {
        return crate::upload::session(socket, request, target).await;
    }
    trace.request_id = request.request_id;
    trace.enabled =
        matches!(&request.payload, Some(RequestPayload::Status(status)) if status.include_timings);
    trace.phase = "prepare";
    let prepared = Instant::now();
    let mut current = identity.clone();
    current.uptime_ms = Instant::now().as_millis();
    let include_boot =
        matches!(&request.payload, Some(RequestPayload::Status(status)) if status.include_boot);
    if !include_boot {
        current.clear_boot();
    }
    if trace.enabled {
        current.set_timings(DeviceTimings {
            request_read_us: trace.read_us,
            decode_us: trace.decode_us,
            prepare_us: 0,
        });
    }
    let mut response = ota_protocol::respond(request, current);
    trace.prepare_us = prepared.elapsed().as_micros();
    if let Some(ResponsePayload::Status(status)) = &mut response.payload {
        if let Some(timing) = status.mut_timings() {
            timing.prepare_us = trace.prepare_us;
        }
    }
    trace.phase = "encode";
    let encoded = Instant::now();
    let bytes = ota_protocol::encode(&response).map_err(|_| ())?;
    trace.encode_us = encoded.elapsed().as_micros();
    trace.phase = "enqueue";
    let enqueued = Instant::now();
    write_all(socket, &(bytes.len() as u32).to_be_bytes()).await?;
    write_all(socket, &bytes).await?;
    trace.enqueue_us = enqueued.elapsed().as_micros();
    trace.phase = "ack_wait";
    let acked = Instant::now();
    socket.flush().await.map_err(|_| ())?;
    trace.ack_wait_us = acked.elapsed().as_micros();
    trace.phase = "close";
    let closed = Instant::now();
    socket.close();
    socket.flush().await.map_err(|_| ())?;
    trace.close_us = closed.elapsed().as_micros();
    trace.phase = "complete";
    Ok(())
}

pub(crate) async fn read_exact(socket: &mut TcpSocket<'_>, mut bytes: &mut [u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        let n = socket.read(bytes).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}

pub(crate) async fn write_all(socket: &mut TcpSocket<'_>, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        let n = socket.write(bytes).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        bytes = &bytes[n..];
    }
    Ok(())
}
