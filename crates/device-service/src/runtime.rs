//! Runtime owned by the service task and borrowed by TCP and discovery.
use core::sync::atomic::{AtomicU8, Ordering};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};

pub const IDLE: u8 = 0;
pub const RECEIVING: u8 = 1;
pub const VERIFIED: u8 = 2;
pub const COMMITTING: u8 = 3;

pub struct Runtime {
    state: AtomicU8,
    pub announce: Signal<CriticalSectionRawMutex, ()>,
}
impl Runtime {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(IDLE),
            announce: Signal::new(),
        }
    }
    pub fn state(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            RECEIVING => "receiving",
            VERIFIED => "verified",
            COMMITTING => "committing",
            _ => "idle",
        }
    }
    pub fn transition(&self, state: u8) {
        self.state.store(state, Ordering::Relaxed);
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
        self.0.transition(IDLE);
    }
}
