//! Runtime owned by the service task and borrowed by TCP and discovery.
use core::cell::Cell;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};

#[derive(Clone, Copy)]
pub enum State {
    Idle,
    Receiving,
    Verified,
    Committing,
}

pub struct Runtime {
    // Both consumers are joined futures within the same executor task.
    state: Cell<State>,
    pub activation_pending: Cell<bool>,
    pub upload_watchdog: device_grpc::watchdog::UploadWatchdog,
    pub reboot: Signal<CriticalSectionRawMutex, ()>,
    pub log_waker: core::cell::RefCell<Option<core::task::Waker>>,
    pub logs: core::cell::RefCell<ota_protocol::logs::LogBuffer>,
    pub announce: Signal<CriticalSectionRawMutex, ()>,
}
impl Runtime {
    pub fn new() -> Self {
        let rng = esp_hal::rng::Rng::new();
        let boot_id = ((rng.random() as u64) << 32 | rng.random() as u64).max(1);
        Self {
            state: Cell::new(State::Idle),
            activation_pending: Cell::new(false),
            upload_watchdog: device_grpc::watchdog::UploadWatchdog::new(),
            reboot: Signal::new(),
            log_waker: core::cell::RefCell::new(None),
            logs: core::cell::RefCell::new(ota_protocol::logs::LogBuffer::new(boot_id)),
            announce: Signal::new(),
        }
    }
    pub fn log(&self, severity: ota_protocol::LogSeverity, message: &str) {
        self.logs
            .borrow_mut()
            .push(embassy_time::Instant::now().as_millis(), severity, message);
        if let Some(waker) = self.log_waker.borrow_mut().take() {
            waker.wake();
        }
    }
    pub fn state(&self) -> &'static str {
        match self.state.get() {
            State::Receiving => "receiving",
            State::Verified => "verified",
            State::Committing => "committing",
            State::Idle => "idle",
        }
    }
    pub fn transition(&self, state: State) {
        self.state.set(state);
        self.log(ota_protocol::LogSeverity::Info, self.state());
        self.announce.signal(());
    }
}
