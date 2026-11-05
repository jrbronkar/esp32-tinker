//! Retained logs and bounded notification for a single-threaded executor.
use core::{cell::RefCell, task::Context};
use embassy_sync::waitqueue::MultiWakerRegistration;
use ota_protocol::{LogSeverity, ReadLogsRequest, ReadLogsResponse, logs::LogBuffer};

/// Cursor-based log storage shared by polling and streaming clients.
///
/// Notifications are hints: readers always consult their cursor again. Embassy
/// deduplicates task wakers and wakes existing readers when registration is full.
/// A cancelled reader may leave a bounded registration until the next append;
/// no connection identifiers or explicit disconnect cleanup are needed.
pub struct LogStore<const READERS: usize> {
    buffer: RefCell<LogBuffer>,
    readers: RefCell<MultiWakerRegistration<READERS>>,
}
impl<const READERS: usize> LogStore<READERS> {
    pub fn new(boot_id: u64) -> Self {
        assert!(READERS > 0, "log store requires notification capacity");
        Self {
            buffer: RefCell::new(LogBuffer::new(boot_id)),
            readers: RefCell::new(MultiWakerRegistration::new()),
        }
    }
    pub fn push(&self, uptime_ms: u64, severity: LogSeverity, message: &str) {
        self.buffer.borrow_mut().push(uptime_ms, severity, message);
        // Release internal borrows before invoking executor-owned wakers.
        let mut readers = self.readers.replace(MultiWakerRegistration::new());
        readers.wake();
    }
    /// Snapshot reads do not change streaming notification registrations.
    pub fn read(&self, cursor: &ReadLogsRequest) -> ReadLogsResponse {
        self.buffer.borrow().read(cursor)
    }
    /// Register before reading so the next append cannot be missed.
    pub fn read_and_subscribe(
        &self,
        cursor: &ReadLogsRequest,
        cx: &mut Context<'_>,
    ) -> ReadLogsResponse {
        self.readers.borrow_mut().register(cx.waker());
        self.read(cursor)
    }
}
