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
    pub announce: Signal<CriticalSectionRawMutex, ()>,
}
impl Runtime {
    pub fn new() -> Self {
        Self {
            state: Cell::new(State::Idle),
            announce: Signal::new(),
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
        self.announce.signal(());
    }
}
pub struct UploadGuard<'a>(&'a Runtime);
impl<'a> UploadGuard<'a> {
    pub fn new(runtime: &'a Runtime) -> Self {
        Self(runtime)
    }
}
impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.0.transition(State::Idle);
    }
}
