//! Two bounded HTTP/2 connections on the single management port.
use core::time::Duration as CoreDuration;
use device_grpc::protolink;
use device_grpc::{Environment, StreamingManagement, UploadEvent};
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
    embassy_futures::join::join(
        serve_connection(stack, identity, runtime, target, target_offset),
        serve_connection(stack, identity, runtime, target, target_offset),
    )
    .await;
}
struct Connection {
    watchdog: device_grpc::watchdog::UploadWatchdog,
    reboot: embassy_sync::signal::Signal<embassy_sync::blocking_mutex::raw::NoopRawMutex, ()>,
    activation_started: core::cell::Cell<bool>,
}
async fn serve_connection(
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
            let connection_state = Connection {
                watchdog: Default::default(),
                reboot: Default::default(),
                activation_started: core::cell::Cell::new(false),
            };
            let mut handler = device_grpc::ManagementServer::new(StreamingManagement::new(
                target,
                target_offset,
                DeviceEnvironment {
                    identity,
                    runtime,
                    connection: &connection_state,
                },
            ));
            {
                let (reader, writer) = socket.split();
                let connection = with_timeout(
                    Duration::from_secs(ota_protocol::MAX_UPLOAD_DURATION_SECS),
                    protolink::server_split(reader, writer)
                        .with_config(device_grpc::config())
                        .with_deadline_timer(EmbassyClock)
                        .build()
                        .serve(&mut handler),
                );
                let reboot = async {
                    connection_state.reboot.wait().await;
                    // Give the driver a bounded opportunity to send the commit ACK and
                    // trailers even when another RPC (such as Logs) remains open.
                    Timer::after_secs(2).await;
                };
                let stop = embassy_futures::select::select(
                    reboot,
                    connection_state.watchdog.expired(&EmbassyClock),
                );
                if let embassy_futures::select::Either::Second(
                    embassy_futures::select::Either::Second(()),
                ) = embassy_futures::select::select(connection, stop).await
                {
                    runtime.log(
                        ota_protocol::LogSeverity::Warn,
                        "gRPC upload inactivity timeout",
                    );
                }
            }
            drop(handler);
            if connection_state.activation_started.get() {
                // Flush bytes already handed to TCP before reset. This is best
                // effort: a lost ACK still leaves commit outcome uncertain.
                let _ = with_timeout(Duration::from_millis(500), socket.flush()).await;
                esp_hal::system::software_reset();
            }
        }
        socket.abort();
        let _ = with_timeout(Duration::from_millis(250), socket.flush()).await;
        Timer::after_millis(10).await;
    }
}

struct DeviceEnvironment<'a> {
    identity: &'a ota_protocol::StatusResponse,
    runtime: &'a crate::runtime::Runtime,
    connection: &'a Connection,
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
        self.runtime.logs.read_and_subscribe(cursor, cx)
    }
    fn now_us(&self) -> u64 {
        Instant::now().as_micros()
    }
    fn activation_pending(&self) -> bool {
        self.runtime.activation_pending.get()
    }
    fn event(&self, event: UploadEvent) {
        match event {
            UploadEvent::Receiving | UploadEvent::ChunkWritten { .. } | UploadEvent::Verified => {
                self.connection
                    .watchdog
                    .activity(CoreDuration::from_micros(Instant::now().as_micros()));
            }
            UploadEvent::Released | UploadEvent::ActivationPending => {
                self.connection.watchdog.clear()
            }
            UploadEvent::Committed => {}
        }
        match event {
            UploadEvent::ActivationPending => {
                self.runtime.activation_pending.set(true);
                self.runtime.transition(crate::runtime::State::Committing);
                self.connection.activation_started.set(true);
                self.connection.reboot.signal(());
            }
            UploadEvent::Receiving => self.runtime.transition(crate::runtime::State::Receiving),
            UploadEvent::Verified => self.runtime.transition(crate::runtime::State::Verified),
            UploadEvent::Released => self.runtime.transition(crate::runtime::State::Idle),
            UploadEvent::ChunkWritten { .. } | UploadEvent::Committed => {}
        }
        use core::fmt::Write;
        let mut message = heapless::String::<64>::new();
        let _ = write!(message, "{event}");
        self.runtime.log(ota_protocol::LogSeverity::Info, &message);
    }
}
