//! Upload inactivity is independent of HTTP/2 activity on other streams.
use core::{cell::Cell, future::Future, time::Duration};
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};

/// Single-task upload lease watchdog. Ordinary log traffic never refreshes it.
pub struct UploadWatchdog {
    deadline: Cell<Option<Duration>>,
    changed: Signal<NoopRawMutex, ()>,
}
impl Default for UploadWatchdog {
    fn default() -> Self {
        Self::new()
    }
}
impl UploadWatchdog {
    pub const fn new() -> Self {
        Self {
            deadline: Cell::new(None),
            changed: Signal::new(),
        }
    }
    pub fn activity(&self, now: Duration) {
        self.deadline.set(Some(
            now.saturating_add(Duration::from_secs(ota_protocol::SESSION_TIMEOUT_SECS)),
        ));
        self.changed.signal(());
    }
    pub fn clear(&self) {
        self.deadline.set(None);
        self.changed.signal(());
    }
    /// End the connection on expiry, even if a response is flow-control blocked.
    /// The driver drop then cancels calls and releases the exclusive flash lease.
    pub async fn expired<Sleep: Future<Output = ()>>(
        &self,
        now: impl Fn() -> Duration,
        sleep_until: impl Fn(Duration) -> Sleep,
    ) {
        loop {
            match self.deadline.get() {
                None => self.changed.wait().await,
                Some(deadline) => {
                    if let Either::First(()) =
                        select(sleep_until(deadline), self.changed.wait()).await
                        && self.deadline.get() == Some(deadline)
                        && now() >= deadline
                    {
                        return;
                    }
                }
            }
        }
    }
}
