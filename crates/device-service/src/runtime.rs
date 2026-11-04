//! Management state observed by TCP status and the independent discovery task.
use core::sync::atomic::{AtomicU8, Ordering};
static STATE: AtomicU8 = AtomicU8::new(0);
pub fn state() -> &'static str {
    match STATE.load(Ordering::Relaxed) {
        1 => "receiving",
        2 => "verified",
        3 => "committing",
        _ => "idle",
    }
}
pub fn transition(state: u8) {
    STATE.store(state, Ordering::Relaxed);
}
pub struct UploadGuard;
impl UploadGuard {
    pub fn new() -> Self {
        Self
    }
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        transition(0);
    }
}
