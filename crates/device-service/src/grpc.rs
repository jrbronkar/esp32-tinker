//! Opt-in cleartext HTTP/2 status experiment; legacy OTA stays available.
use core::time::Duration as CoreDuration;
use device_grpc::protolink;
use device_grpc::{Environment, StreamingManagement};
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
    target: &core::cell::RefCell<crate::flash_target::FlashTarget>,
    target_offset: u32,
) {
    let mut rx = [0; 1024];
    let mut tx = [0; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    loop {
        stack.wait_config_up().await;
        if socket.accept(device_grpc::PORT).await.is_ok() {
            let mut handler = StreamingManagement::new(
                target,
                target_offset,
                DeviceEnvironment { identity, runtime },
            );
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
        let stats = esp_alloc::HEAP.stats();
        use core::fmt::Write;
        let mut heap = heapless::String::<64>::new();
        let _ = write!(
            heap,
            "gRPC heap used={} peak={} free={}",
            stats.current_usage,
            stats.max_usage,
            stats.size - stats.current_usage
        );
        runtime.log(ota_protocol::LogSeverity::Info, &heap);
        socket.abort();
        let _ = with_timeout(Duration::from_millis(250), socket.flush()).await;
        Timer::after_millis(10).await;
    }
}

struct DeviceEnvironment<'a> {
    identity: &'a ota_protocol::StatusResponse,
    runtime: &'a crate::runtime::Runtime,
}
impl Environment for DeviceEnvironment<'_> {
    fn status(&self) -> ota_protocol::StatusResponse {
        let mut status = self.identity.clone();
        status.uptime_ms = Instant::now().as_millis();
        status.management_state = self.runtime.state().try_into().unwrap_or_default();
        status
    }
    fn logs(
        &self,
        cursor: &ota_protocol::ReadLogsRequest,
        cx: &mut core::task::Context<'_>,
    ) -> ota_protocol::ReadLogsResponse {
        *self.runtime.log_waker.borrow_mut() = Some(cx.waker().clone());
        self.runtime.logs.borrow().read(cursor)
    }
    fn event(&self, message: &str) {
        if message == "gRPC upload receiving" {
            self.runtime.transition(crate::runtime::State::Receiving);
        }
        if message == "gRPC image verified" {
            self.runtime.transition(crate::runtime::State::Verified);
        }
        if message == "gRPC upload released" {
            self.runtime.transition(crate::runtime::State::Idle);
        }
        self.runtime.log(ota_protocol::LogSeverity::Info, message);
    }
}
