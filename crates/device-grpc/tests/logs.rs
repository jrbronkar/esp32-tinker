use device_grpc::logs::LogStore;
use ota_protocol::{LogSeverity, ReadLogsRequest};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
};
#[derive(Default)]
struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn reader() -> (Arc<Counter>, Waker) {
    let counter = Arc::new(Counter::default());
    (counter.clone(), Waker::from(counter))
}
#[test]
fn append_wakes_distinct_readers_and_snapshot_preserves_registrations() {
    let logs = LogStore::<2>::new(1);
    let (first, first_waker) = reader();
    let (second, second_waker) = reader();
    let cursor = ReadLogsRequest::default();
    for _ in 0..3 {
        assert!(
            logs.read_and_subscribe(&cursor, &mut Context::from_waker(&first_waker))
                .entries
                .is_empty()
        );
    }
    logs.read_and_subscribe(&cursor, &mut Context::from_waker(&second_waker));
    assert!(logs.read(&cursor).entries.is_empty());
    logs.push(1, LogSeverity::Info, "hello");
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    assert_eq!(logs.read(&cursor).entries[0].message, "hello");
    logs.push(2, LogSeverity::Info, "no readers waiting");
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
}
#[test]
fn registration_capacity_wakes_existing_readers_instead_of_losing_notifications() {
    let logs = LogStore::<1>::new(1);
    let (first, first_waker) = reader();
    let (second, second_waker) = reader();
    let cursor = ReadLogsRequest::default();
    logs.read_and_subscribe(&cursor, &mut Context::from_waker(&first_waker));
    drop(first_waker); // A cancelled reader leaves only a bounded notification.
    logs.read_and_subscribe(&cursor, &mut Context::from_waker(&second_waker));
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
    logs.push(1, LogSeverity::Info, "hello");
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    assert_eq!(logs.read(&cursor).entries.len(), 1);
}
