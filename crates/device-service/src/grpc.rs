//! Opt-in cleartext HTTP/2 status experiment; legacy OTA stays available.
use core::time::Duration as CoreDuration;
use device_grpc::protolink;
use device_grpc::Management;
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{with_timeout, Duration, Instant, Timer};

struct EmbassyClock;
impl protolink::Clock for EmbassyClock {
    fn now(&self) -> CoreDuration {
        CoreDuration::from_micros(Instant::now().as_micros())
    }
}
impl protolink::Timer for EmbassyClock {
    async fn sleep_until(&self, deadline: CoreDuration) {
        Timer::at(Instant::from_micros(
            deadline.as_micros().min(u64::MAX as u128) as u64,
        ))
        .await;
    }
}

pub async fn serve(
    stack: Stack<'static>,
    identity: &ota_protocol::StatusResponse,
    runtime: &crate::runtime::Runtime,
) {
    let mut rx = [0; 1024];
    let mut tx = [0; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    loop {
        stack.wait_config_up().await;
        if socket.accept(device_grpc::PORT).await.is_ok() {
            let mut handler = Management(|| {
                let mut status = identity.clone();
                status.uptime_ms = Instant::now().as_millis();
                status.management_state.clear();
                let _ = status.management_state.push_str(runtime.state());
                status
            });
            let (reader, writer) = socket.split();
            let _ = with_timeout(
                Duration::from_secs(180),
                protolink::server_split(reader, writer)
                    .with_config(device_grpc::config())
                    .with_deadline_timer(EmbassyClock)
                    .build()
                    .serve(&mut handler),
            )
            .await;
        }
        socket.abort();
        let _ = with_timeout(Duration::from_millis(250), socket.flush()).await;
        Timer::after_millis(10).await;
    }
}
