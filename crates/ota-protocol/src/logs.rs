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
        assert!(boot_id != 0);
        Self {
            boot_id,
            next: 1,
            entries: heapless::Deque::new(),
        }
    }
    pub fn push(&mut self, uptime_ms: u64, severity: LogSeverity, message: &str) {
        // Truncate at a UTF-8 boundary rather than dropping long messages.
        let mut end = message.len().min(64);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        let entry = LogEntry {
            sequence: self.next,
            uptime_ms,
            severity,
            message: message[..end].try_into().unwrap(),
        };
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
        assert_eq!(first.entries[0].sequence, 9);
        assert_eq!(first.next_sequence, 12);
        let next = logs.read(&ReadLogsRequest {
            boot_id: 7,
            after_sequence: 12,
        });
        assert!(!next.dropped);
        assert_eq!(next.entries[0].sequence, 13);
        let reset = logs.read(&ReadLogsRequest {
            boot_id: 6,
            after_sequence: 999,
        });
        assert!(reset.reset);
        assert_eq!(reset.entries[0].sequence, 9);
    }
    #[test]
    fn worst_case_batch_fits_control_frame_and_utf8_is_preserved() {
        let mut logs = LogBuffer::new(u64::MAX);
        logs.next = u64::MAX - 5;
        for _ in 0..4 {
            logs.push(u64::MAX, LogSeverity::Error, &"é".repeat(40));
        }
        let response = crate::Response {
            protocol_version: 1,
            request_id: u32::MAX,
            payload: Some(crate::ResponsePayload::Logs(
                logs.read(&ReadLogsRequest::default()),
            )),
        };
        let bytes = crate::encode(&response).unwrap();
        assert_eq!(crate::decode::<crate::Response>(&bytes).unwrap(), response);
    }
}
