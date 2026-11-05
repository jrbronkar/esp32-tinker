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
    pub logs: device_grpc::logs::LogStore<2>,
    pub announce: Signal<CriticalSectionRawMutex, ()>,
}
impl Runtime {
    pub fn new() -> Self {
        let rng = esp_hal::rng::Rng::new();
        let boot_id = ((rng.random() as u64) << 32 | rng.random() as u64).max(1);
        Self {
            state: Cell::new(State::Idle),
            activation_pending: Cell::new(false),
            logs: device_grpc::logs::LogStore::new(boot_id),
            announce: Signal::new(),
        }
    }
    pub fn log(&self, severity: ota_protocol::LogSeverity, message: &str) {
        self.logs
            .push(embassy_time::Instant::now().as_millis(), severity, message);
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
