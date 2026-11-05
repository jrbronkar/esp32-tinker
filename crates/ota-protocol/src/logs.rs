//! Fixed-memory log storage, independent of the firmware executor.
use crate::{LogEntry, LogSeverity, ReadLogsRequest, ReadLogsResponse};

pub const LOG_CAPACITY: usize = 32;
pub struct LogBuffer {
    boot_id: u64,
    next: u64,
    entries: heapless::Deque<LogEntry, LOG_CAPACITY>,
}
impl LogBuffer {
    pub fn new(boot_id: u64) -> Self {
        Self {
            boot_id: boot_id.max(1),
            next: 1,
            entries: heapless::Deque::new(),
        }
    }
    pub fn push(&mut self, uptime_ms: u64, severity: LogSeverity, message: &str) {
        // Start a new cursor epoch before sequence overflow.
        if self.next == u64::MAX {
            self.boot_id = self.boot_id.wrapping_add(1).max(1);
            self.next = 1;
            self.entries.clear();
        }
        let mut entry = LogEntry {
            sequence: self.next,
            uptime_ms,
            severity,
            ..Default::default()
        };
        // Stop at the generated field's capacity, preserving UTF-8 boundaries.
        for ch in message.chars() {
            if entry.message.push(ch).is_err() {
                break;
            }
        }
        self.next += 1;
        if self.entries.is_full() {
            self.entries.pop_front();
        }
        let _ = self.entries.push_back(entry);
    }
    pub fn read(&self, request: &ReadLogsRequest) -> ReadLogsResponse {
        let reset = (request.boot_id != 0 && request.boot_id != self.boot_id)
            || request.after_sequence >= self.next;
        let after = if reset { 0 } else { request.after_sequence };
        let oldest = self.entries.front().map_or(self.next, |e| e.sequence);
        let mut result = ReadLogsResponse {
            boot_id: self.boot_id,
            reset,
            dropped: after.saturating_add(1) < oldest,
            next_sequence: after,
            ..Default::default()
        };
        for entry in self.entries.iter().filter(|e| e.sequence > after).take(4) {
            result.next_sequence = entry.sequence;
            let _ = result.entries.push(entry.clone());
        }
        result.more = result.next_sequence < self.next - 1;
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pagination_loss_and_reboot() {
        let mut logs = LogBuffer::new(7);
        for i in 0..40 {
            logs.push(i, LogSeverity::Info, "hello");
        }
        let first = logs.read(&ReadLogsRequest::default());
        assert!(first.dropped);
        assert!(first.more);
        assert_eq!(first.entries[0].sequence, 9);
        assert_eq!(first.next_sequence, 12);
        let next = logs.read(&ReadLogsRequest {
            boot_id: 7,
            after_sequence: 12,
        });
        assert!(!next.dropped);
        assert_eq!(next.entries[0].sequence, 13);
        let final_batch = logs.read(&ReadLogsRequest {
            boot_id: 7,
            after_sequence: 36,
        });
        assert_eq!(final_batch.entries.len(), 4);
        assert!(!final_batch.more);
        let reset = logs.read(&ReadLogsRequest {
            boot_id: 6,
            after_sequence: 999,
        });
        assert!(reset.reset);
        assert_eq!(reset.entries[0].sequence, 9);
    }
    #[test]
    fn zero_boot_id_and_sequence_exhaustion_do_not_panic() {
        let mut logs = LogBuffer::new(0);
        assert_eq!(logs.read(&ReadLogsRequest::default()).boot_id, 1);
        logs.next = u64::MAX;
        logs.push(0, LogSeverity::Info, "new epoch");
        let response = logs.read(&ReadLogsRequest {
            boot_id: 1,
            after_sequence: u64::MAX - 1,
        });
        assert!(response.reset);
        assert_eq!(response.boot_id, 2);
        assert_eq!(response.entries[0].sequence, 1);
    }
    #[test]
    fn worst_case_batch_fits_control_frame_and_utf8_is_preserved() {
        let mut logs = LogBuffer::new(u64::MAX);
        logs.next = u64::MAX - 5;
        for _ in 0..4 {
            logs.push(u64::MAX, LogSeverity::Error, &"é".repeat(40));
        }
        let response = logs.read(&ReadLogsRequest::default());
        let bytes = crate::encode(&response).unwrap();
        assert_eq!(crate::decode::<ReadLogsResponse>(&bytes).unwrap(), response);
    }
}
