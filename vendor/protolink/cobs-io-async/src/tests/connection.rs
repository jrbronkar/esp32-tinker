use core::convert::Infallible;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Wake,
    vec,
    vec::Vec,
};

use super::support::clock::FakeClock;
use super::support::link::{Endpoint, MockError, Trickle, Wire, is_ack, is_data, link, parse_wire};
use crate::connection::STEP_BUDGET;
use crate::protocol::{self, ACK_FRAME_LEN};
use crate::{
    Config, ConfigError, ConnectionError, PendingOperation, Reliable, Timer, Transport,
    max_encoding_length, sync,
};

const SESSION: u64 = 0x0123_4567_89AB_CDEF;
const N: usize = 64;
const RTO: u64 = 100;
const TICK: u64 = 10;
const RETRIES: u32 = 5;
const MAX_TICKS: usize = 20_000;

type Conn = Reliable<Endpoint, FakeClock, N>;
type Failure = ConnectionError<MockError>;

fn config(session: u64) -> Config {
    Config {
        session,
        retransmit_timeout: RTO,
        max_retries: RETRIES,
    }
}

struct Sim {
    a: Conn,
    b: Conn,
    ha: Endpoint,
    hb: Endpoint,
    clock: FakeClock,
}

fn sim() -> Sim {
    sim_with(config(SESSION), config(SESSION))
}

fn sim_with(a: Config, b: Config) -> Sim {
    let (ea, eb) = link();
    let clock = FakeClock::default();
    Sim {
        ha: ea.clone(),
        hb: eb.clone(),
        a: Reliable::new(ea, clock.clone(), a).unwrap(),
        b: Reliable::new(eb, clock.clone(), b).unwrap(),
        clock,
    }
}

impl Sim {
    fn tick(&mut self) {
        self.clock.advance(TICK);
        let _ = progress(&mut self.a);
        let _ = progress(&mut self.b);
    }

    fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.tick();
        }
    }
}

fn with_cx<R>(f: impl FnOnce(&mut Context<'_>) -> R) -> R {
    f(&mut Context::from_waker(Waker::noop()))
}

fn write(conn: &mut Conn, buf: &[u8]) -> Poll<Result<usize, Failure>> {
    with_cx(|cx| conn.poll_write(cx, buf))
}

fn read(conn: &mut Conn, buf: &mut [u8]) -> Poll<Result<usize, Failure>> {
    with_cx(|cx| conn.poll_read(cx, buf))
}

fn flush(conn: &mut Conn) -> Poll<Result<(), Failure>> {
    with_cx(|cx| conn.poll_flush(cx))
}

fn progress(conn: &mut Conn) -> Poll<Failure> {
    with_cx(|cx| conn.poll_progress(cx))
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

fn try_write(conn: &mut Conn, data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    match write(conn, data) {
        Poll::Ready(Ok(len)) => len,
        Poll::Ready(Err(error)) => panic!("write failed: {error:?}"),
        Poll::Pending => 0,
    }
}

fn try_read(conn: &mut Conn, buf: &mut [u8], out: &mut Vec<u8>) {
    match read(conn, buf) {
        Poll::Ready(Ok(len)) => out.extend_from_slice(&buf[..len]),
        Poll::Ready(Err(error)) => panic!("read failed: {error:?}"),
        Poll::Pending => {}
    }
}

/// Moves `ab` from A to B and `ba` from B to A, returning what B and A received.
fn transfer(sim: &mut Sim, ab: &[u8], ba: &[u8], read_size: usize) -> (Vec<u8>, Vec<u8>) {
    let (mut sent_a, mut sent_b) = (0, 0);
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    let mut buf = vec![0u8; read_size];
    for _ in 0..MAX_TICKS {
        sent_a += try_write(&mut sim.a, &ab[sent_a..]);
        sent_b += try_write(&mut sim.b, &ba[sent_b..]);
        try_read(&mut sim.b, &mut buf, &mut got_b);
        try_read(&mut sim.a, &mut buf, &mut got_a);
        sim.tick();
        assert!(got_b.len() <= ab.len() && got_a.len() <= ba.len());
        if got_b.len() == ab.len()
            && got_a.len() == ba.len()
            && flush(&mut sim.a).is_ready()
            && flush(&mut sim.b).is_ready()
        {
            return (got_b, got_a);
        }
    }
    panic!("transfer did not complete");
}

fn count(frames: &[Vec<u8>], keep: impl Fn(&Wire) -> bool) -> usize {
    frames
        .iter()
        .filter_map(|frame| parse_wire(frame))
        .filter(|wire| keep(wire))
        .count()
}

fn corrupt(wire: &[u8], index: usize) -> Vec<u8> {
    let mut wire = wire.to_vec();
    wire[index] = if wire[index] == 0xFF {
        0xFE
    } else {
        wire[index] + 1
    };
    wire
}

fn drop_first(matches: fn(&[u8]) -> bool) -> impl FnMut(usize, &[u8]) -> Vec<u8> {
    let mut dropped = false;
    move |_, wire| {
        if !dropped && matches(wire) {
            dropped = true;
            Vec::new()
        } else {
            wire.to_vec()
        }
    }
}

fn ack_wire(session: u64, seq: u32) -> Vec<u8> {
    let mut frame = [0u8; ACK_FRAME_LEN];
    let len = protocol::encode_ack(session, seq, &mut frame);
    frame[..len].to_vec()
}

fn data_wire(session: u64, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut scratch = [0u8; N];
    let mut frame = [0u8; N];
    let len = protocol::encode_data(session, seq, payload, &mut scratch, &mut frame);
    frame[..len].to_vec()
}

struct Flag(AtomicUsize);

impl Flag {
    fn new() -> (Arc<Self>, Waker) {
        let flag = Arc::new(Self(AtomicUsize::new(0)));
        (flag.clone(), Waker::from(flag))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn rejects_capacity_without_room_for_a_packet() {
    let (endpoint, _) = link();
    let result = Reliable::<_, _, 20>::new(endpoint.clone(), FakeClock::default(), config(1));
    assert!(matches!(result, Err(ConfigError::BufferTooSmall)));
    let result = Reliable::<_, _, 21>::new(endpoint, FakeClock::default(), config(1));
    assert!(result.is_ok());
    assert_eq!(Reliable::<Endpoint, FakeClock, 21>::MAX_PAYLOAD, 1);
}

#[test]
fn ordered_transfer_across_partial_writes() {
    let mut sim = sim();
    let data = pattern(1000);
    let Poll::Ready(Ok(first)) = write(&mut sim.a, &data) else {
        panic!("first write not accepted");
    };
    assert_eq!(first, Conn::MAX_PAYLOAD);
    assert!(first < data.len());
    let mut sim = self::sim();
    let (received, _) = transfer(&mut sim, &data, &[], 256);
    assert_eq!(received, data);
}

#[test]
fn small_reads_span_packet_boundaries() {
    for read_size in [1, 3, 7, 44, 45] {
        let mut sim = sim();
        let data = pattern(300);
        let (received, _) = transfer(&mut sim, &data, &[], read_size);
        assert_eq!(received, data, "read size {read_size}");
    }
}

#[test]
fn byte_at_a_time_transport() {
    let mut sim = sim();
    for endpoint in [&sim.ha, &sim.hb] {
        endpoint.set_read_chunk(1);
        endpoint.set_write_chunk(1);
    }
    let (ab, ba) = (pattern(200), pattern(150));
    let (to_b, to_a) = transfer(&mut sim, &ab, &ba, 16);
    assert_eq!((to_b, to_a), (ab, ba));
}

#[test]
fn empty_buffers_return_zero_without_traffic() {
    let mut sim = sim();
    assert!(matches!(write(&mut sim.a, &[]), Poll::Ready(Ok(0))));
    assert!(matches!(read(&mut sim.a, &mut []), Poll::Ready(Ok(0))));
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    sim.ticks(50);
    assert!(sim.ha.written_frames().is_empty());
    assert!(sim.hb.written_frames().is_empty());
}

#[test]
fn outgoing_buffer_stays_occupied_until_acknowledged() {
    let mut sim = sim();
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    assert!(write(&mut sim.a, &[2]).is_pending());
    assert!(flush(&mut sim.a).is_pending());
    sim.ticks(3);
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(1))));
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert!(matches!(write(&mut sim.a, &[2]), Poll::Ready(Ok(1))));
}

#[test]
fn lost_frames_are_retransmitted_without_duplicate_delivery() {
    for (name, lose_ack) in [("lost data", false), ("lost ack", true)] {
        let mut sim = sim();
        if lose_ack {
            sim.hb.filter_outgoing(drop_first(is_ack));
        } else {
            sim.ha.filter_outgoing(drop_first(is_data));
        }
        let data = pattern(100);
        let (received, _) = transfer(&mut sim, &data, &[], 32);
        assert_eq!(received, data, "{name}");
        let sent = sim.ha.written_frames();
        let datas = count(&sent, |w| matches!(w, Wire::Data { seq: 0, .. }));
        assert_eq!(datas, 2, "{name}");
        if lose_ack {
            let acks = sim.hb.written_frames();
            assert_eq!(
                count(&acks, |w| matches!(w, Wire::Ack { seq: 0, .. })),
                2,
                "{name}"
            );
        }
    }
}

#[test]
fn corrupted_ack_does_not_release_the_outgoing_buffer() {
    let mut sim = sim();
    let mut corrupted = false;
    sim.hb.filter_outgoing(move |_, wire| {
        if !corrupted && is_ack(wire) {
            corrupted = true;
            corrupt(wire, 3)
        } else {
            wire.to_vec()
        }
    });
    assert!(matches!(write(&mut sim.a, &[1, 2, 3]), Poll::Ready(Ok(3))));
    sim.ticks(5);
    assert!(flush(&mut sim.a).is_pending());
    assert!(write(&mut sim.a, &[4]).is_pending());
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(3))));
    sim.ticks((RTO / TICK) as usize + 2);
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert!(read(&mut sim.b, &mut buf).is_pending());
}

#[test]
fn foreign_acks_cannot_release_the_outgoing_buffer() {
    let (mut a, ea, _) = solo(RETRIES);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    for wire in [
        ack_wire(SESSION + 1, 0),
        ack_wire(SESSION, 1),
        ack_wire(SESSION, u32::MAX),
        corrupt(&ack_wire(SESSION, 0), 2),
    ] {
        ea.inject_incoming(&wire);
        let _ = progress(&mut a);
        assert!(flush(&mut a).is_pending());
        assert!(write(&mut a, &[2]).is_pending());
    }
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert!(matches!(flush(&mut a), Poll::Ready(Ok(()))));
}

#[test]
fn full_receive_buffer_does_not_block_outgoing_traffic() {
    let mut sim = sim();
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    sim.ticks(3);
    assert!(matches!(write(&mut sim.b, &[9, 9]), Poll::Ready(Ok(2))));
    sim.ticks(3);
    assert!(matches!(flush(&mut sim.b), Poll::Ready(Ok(()))));
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.a, &mut buf), Poll::Ready(Ok(2))));
}

#[test]
fn partial_reads_keep_the_slot_occupied() {
    let mut sim = sim();
    assert!(matches!(
        write(&mut sim.a, &[1, 2, 3, 4]),
        Poll::Ready(Ok(4))
    ));
    sim.ticks(3);
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert!(matches!(write(&mut sim.a, &[5]), Poll::Ready(Ok(1))));
    sim.ticks(3);
    assert!(flush(&mut sim.a).is_pending());
    assert_eq!(
        count(&sim.hb.written_frames(), |w| matches!(w, Wire::Ack { .. })),
        1
    );
    let mut buf = [0u8; 2];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(2))));
    assert_eq!(buf, [1, 2]);
    sim.ticks(3);
    assert!(flush(&mut sim.a).is_pending());
    assert_eq!(
        count(&sim.hb.written_frames(), |w| matches!(w, Wire::Ack { .. })),
        1
    );
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(2))));
    assert_eq!(buf, [3, 4]);
    sim.ticks((RTO / TICK) as usize + 3);
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(1))));
    assert_eq!(buf[0], 5);
}

#[test]
fn recovers_from_oversized_and_unterminated_frames() {
    let junk: [&[u8]; 6] = [
        &[0x55; 300],
        &[0x55; 63],
        &[0x55; 64],
        &[7, 7, 7],
        &[5, 1, 0],
        &[0xFF; 100],
    ];
    for junk in junk {
        let mut sim = sim();
        sim.hb.inject_incoming(junk);
        let data = pattern(100);
        let (received, _) = transfer(&mut sim, &data, &[], 32);
        assert_eq!(received, data, "junk {junk:?}");
    }
}

#[test]
fn recovers_from_junk_between_frames() {
    let mut sim = sim();
    sim.ha.filter_outgoing(|_, wire| {
        let mut out = wire.to_vec();
        out.extend_from_slice(&[0x55; 300]);
        out
    });
    sim.hb.filter_outgoing(|_, wire| {
        let mut out = vec![9, 9, 9, 0];
        out.extend_from_slice(wire);
        out
    });
    let data = pattern(200);
    let (received, _) = transfer(&mut sim, &data, &[], 32);
    assert_eq!(received, data);
}

#[test]
fn bytes_after_a_completed_frame_are_processed() {
    let (_ea, eb) = link();
    let clock = FakeClock::default();
    let mut b: Conn = Reliable::new(eb.clone(), clock, config(SESSION)).unwrap();
    assert!(matches!(write(&mut b, &[9]), Poll::Ready(Ok(1))));
    assert!(flush(&mut b).is_pending());
    let mut input = ack_wire(SESSION, 0);
    input.extend_from_slice(&data_wire(SESSION, 0, &[4, 5, 6]));
    eb.inject_incoming(&input);
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut b, &mut buf), Poll::Ready(Ok(3))));
    assert_eq!(&buf[..3], &[4, 5, 6]);
    assert!(matches!(flush(&mut b), Poll::Ready(Ok(()))));
    assert_eq!(
        count(&eb.written_frames(), |w| matches!(
            w,
            Wire::Ack { seq: 0, .. }
        )),
        1
    );
}

#[test]
fn simultaneous_bidirectional_transfer() {
    let mut sim = sim();
    let (ab, ba) = (
        pattern(700),
        pattern(500).into_iter().rev().collect::<Vec<_>>(),
    );
    let (to_b, to_a) = transfer(&mut sim, &ab, &ba, 20);
    assert_eq!(to_b, ab);
    assert_eq!(to_a, ba);
}

#[test]
fn frames_are_never_interleaved() {
    let mut sim = sim();
    for endpoint in [&sim.ha, &sim.hb] {
        endpoint.set_write_chunk(3);
        endpoint.set_read_chunk(5);
    }
    let (ab, ba) = (pattern(400), pattern(400));
    let (to_b, to_a) = transfer(&mut sim, &ab, &ba, 64);
    assert_eq!((to_b, to_a), (ab, ba));
    for endpoint in [&sim.ha, &sim.hb] {
        for frame in endpoint.written_frames() {
            assert!(parse_wire(&frame).is_some(), "malformed frame {frame:?}");
        }
    }
}

#[test]
fn write_resumes_at_the_correct_offset() {
    let mut sim = sim();
    sim.ha.set_write_chunk(4);
    sim.ha.limit_writes(Some(9));
    assert!(matches!(
        write(&mut sim.a, &[1, 2, 3, 4, 5, 6]),
        Poll::Ready(Ok(6))
    ));
    sim.ticks(3);
    assert!(sim.ha.written_frames().is_empty());
    sim.ha.limit_writes(Some(3));
    sim.ticks(3);
    assert!(sim.ha.written_frames().is_empty());
    sim.ha.limit_writes(None);
    sim.ticks(5);
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(6))));
    assert_eq!(&buf[..6], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(data_frames(&sim.ha), 1);
    assert_eq!(sim.ha.written_frames().len(), 1);
}

#[test]
fn retry_exhaustion_is_persistent() {
    for retries in [0, RETRIES] {
        let (mut a, ea, clock) = solo(retries);
        let expected = 1 + retries as usize;
        assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
        let mut result = Poll::Pending;
        for _ in 0..200 {
            clock.advance(TICK);
            result = flush(&mut a);
            if result.is_ready() {
                break;
            }
        }
        assert!(
            matches!(result, Poll::Ready(Err(ConnectionError::Timeout))),
            "retries {retries}"
        );
        assert_eq!(data_frames(&ea), expected, "retries {retries}");
        assert!(matches!(
            write(&mut a, &[2]),
            Poll::Ready(Err(ConnectionError::Failed))
        ));
        assert!(matches!(
            flush(&mut a),
            Poll::Ready(Err(ConnectionError::Failed))
        ));
        let mut buf = [0u8; 4];
        assert!(matches!(
            read(&mut a, &mut buf),
            Poll::Ready(Err(ConnectionError::Failed))
        ));
        assert!(matches!(
            progress(&mut a),
            Poll::Ready(ConnectionError::Failed)
        ));
        clock.advance(10 * RTO);
        assert_eq!(data_frames(&ea), expected, "retries {retries}");
    }
}

#[test]
fn transport_errors_are_terminal() {
    let error = MockError("boom");

    let mut sim = sim();
    sim.ha.fail_next_read(error);
    assert!(
        matches!(progress(&mut sim.a), Poll::Ready(ConnectionError::Transport(e)) if e == error)
    );
    assert!(matches!(
        progress(&mut sim.a),
        Poll::Ready(ConnectionError::Failed)
    ));

    let cases: [(&str, fn(&Endpoint, MockError)); 2] = [
        ("write error", Endpoint::fail_next_write),
        ("flush error", Endpoint::fail_next_flush),
    ];
    for (name, fail) in cases {
        let mut sim = self::sim();
        fail(&sim.ha, error);
        assert!(
            matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))),
            "{name}"
        );
        assert!(
            matches!(flush(&mut sim.a), Poll::Ready(Err(ConnectionError::Transport(e))) if e == error),
            "{name}"
        );
        assert!(
            matches!(
                write(&mut sim.a, &[1]),
                Poll::Ready(Err(ConnectionError::Failed))
            ),
            "{name}"
        );
    }

    let mut sim = self::sim();
    sim.ha.accept_zero_bytes(true);
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    assert!(matches!(
        flush(&mut sim.a),
        Poll::Ready(Err(ConnectionError::WriteZero))
    ));
}

#[test]
fn eof_fails_the_connection_after_buffered_data_is_read() {
    let mut sim = sim();
    sim.hb.inject_incoming(&data_wire(SESSION, 0, &[1, 2, 3]));
    sim.hb.close_incoming();
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(3))));
    assert!(matches!(
        read(&mut sim.b, &mut buf),
        Poll::Ready(Err(ConnectionError::UnexpectedEof))
    ));
    assert!(matches!(
        read(&mut sim.b, &mut buf),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
    assert!(matches!(
        write(&mut sim.b, &[1]),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
}

#[test]
fn session_mismatch_is_ignored() {
    let mut sim = sim_with(config(SESSION), config(SESSION + 1));
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    let mut buf = [0u8; 8];
    let mut result = Poll::Pending;
    for _ in 0..200 {
        sim.clock.advance(TICK);
        let _ = progress(&mut sim.b);
        assert!(read(&mut sim.b, &mut buf).is_pending());
        result = flush(&mut sim.a);
        if result.is_ready() {
            break;
        }
    }
    assert!(matches!(result, Poll::Ready(Err(ConnectionError::Timeout))));
    assert!(sim.hb.written_frames().is_empty());
}

#[test]
fn sequence_exhaustion_is_reported() {
    let mut sim = sim();
    sim.a.set_sequences(u32::MAX as u64, 0);
    sim.b.set_sequences(0, u32::MAX as u64);
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    assert!(write(&mut sim.a, &[2]).is_pending());
    sim.ticks(3);
    let mut buf = [0u8; 8];
    assert!(matches!(read(&mut sim.b, &mut buf), Poll::Ready(Ok(1))));
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert!(matches!(
        write(&mut sim.a, &[2]),
        Poll::Ready(Err(ConnectionError::SequenceExhausted))
    ));
    assert!(matches!(
        write(&mut sim.a, &[2]),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
}

#[test]
fn dropped_futures_do_not_poison_the_connection() {
    let mut sim = sim();
    let mut buf = [0u8; 8];
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..3 {
        let mut future = pin!(poll_fn(|cx| sim.b.poll_read(cx, &mut buf)));
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert!(matches!(write(&mut sim.a, &[1, 2]), Poll::Ready(Ok(2))));
    for _ in 0..3 {
        let mut future = pin!(poll_fn(|cx| sim.a.poll_flush(cx)));
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    let mut future = pin!(poll_fn(|cx| sim.b.poll_read(cx, &mut buf)));
    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(2))));
    sim.ticks(3);
    assert!(matches!(flush(&mut sim.a), Poll::Ready(Ok(()))));
    assert_eq!(&buf[..2], &[1, 2]);
}

#[test]
fn retransmission_deadline_registers_a_wakeup() {
    let (mut a, ea, clock) = solo(RETRIES);
    let (flag, waker) = Flag::new();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(a.poll_write(&mut cx, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_flush(&mut cx).is_pending());
    assert_eq!(clock.deadlines(), vec![RTO]);
    assert_eq!(flag.count(), 0);
    clock.advance(RTO - 1);
    assert_eq!(flag.count(), 0);
    clock.advance(1);
    assert!(flag.count() >= 1);
    assert!(a.poll_flush(&mut cx).is_pending());
    assert_eq!(data_frames(&ea), 2);
    assert_eq!(clock.deadlines(), vec![2 * RTO]);
}

#[test]
fn input_registers_a_wakeup() {
    let mut sim = sim();
    let (flag, waker) = Flag::new();
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 8];
    assert!(sim.b.poll_read(&mut cx, &mut buf).is_pending());
    assert_eq!(flag.count(), 0);
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    assert_eq!(flag.count(), 1);
    assert!(matches!(
        sim.b.poll_read(&mut cx, &mut buf),
        Poll::Ready(Ok(1))
    ));
}

#[test]
fn blocked_input_registers_a_wakeup() {
    let mut sim = sim();
    let (flag, waker) = Flag::new();
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 8];
    sim.hb.block_reads(true);
    assert!(sim.b.poll_read(&mut cx, &mut buf).is_pending());
    assert!(matches!(write(&mut sim.a, &[1]), Poll::Ready(Ok(1))));
    assert_eq!(flag.count(), 0);
    sim.hb.block_reads(false);
    assert_eq!(flag.count(), 1);
    assert!(matches!(
        sim.b.poll_read(&mut cx, &mut buf),
        Poll::Ready(Ok(1))
    ));
}

#[test]
fn blocked_output_registers_a_wakeup() {
    let (mut a, ea, _) = solo(RETRIES);
    let (flag, waker) = Flag::new();
    let mut cx = Context::from_waker(&waker);
    ea.block_writes(true);
    assert!(matches!(a.poll_write(&mut cx, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_flush(&mut cx).is_pending());
    assert_eq!(flag.count(), 0);
    ea.block_writes(false);
    assert!(flag.count() >= 1);
}

#[test]
fn ack_deadline_starts_after_the_flush_completes() {
    let (mut a, ea, clock) = solo(RETRIES);
    ea.block_flush(true);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    for _ in 0..50 {
        clock.advance(10 * RTO);
        assert!(flush(&mut a).is_pending());
    }
    assert!(clock.deadlines().is_empty());
    assert_eq!(data_frames(&ea), 1);
    ea.block_flush(false);
    let _ = progress(&mut a);
    assert_eq!(clock.deadlines(), vec![clock.now() + RTO]);
}

struct Flood {
    reads: usize,
}

impl Transport for Flood {
    type Error = Infallible;

    fn poll_read(
        &mut self,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Infallible>> {
        self.reads += 1;
        buf.fill(0x55);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_write(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Infallible>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn an_always_ready_transport_cannot_monopolize_the_executor() {
    let mut conn: Reliable<Flood, FakeClock, N> =
        Reliable::new(Flood { reads: 0 }, FakeClock::default(), config(SESSION)).unwrap();
    let (flag, waker) = Flag::new();
    let mut cx = Context::from_waker(&waker);
    assert!(conn.poll_progress(&mut cx).is_pending());
    assert_eq!(flag.count(), 1);
    let (flood, _) = conn.into_parts();
    assert!(flood.reads <= STEP_BUDGET);
}

#[test]
fn encoded_frames_fit_the_configured_capacity() {
    for payload in [1, 2, Conn::MAX_PAYLOAD - 1, Conn::MAX_PAYLOAD] {
        for fill in [0u8, 1, 0xFF] {
            let data = vec![fill; payload];
            let wire = data_wire(SESSION, 0, &data);
            assert!(wire.len() <= N);
            assert!(wire.len() <= max_encoding_length(payload + 17) + 2);
            let mut decoded = [0u8; N];
            assert!(sync::decode_to_slice(&wire, &mut decoded).is_ok());
        }
    }
}

fn solo(max_retries: u32) -> (Conn, Endpoint, FakeClock) {
    let (ea, _eb) = link();
    let clock = FakeClock::default();
    let config = Config {
        max_retries,
        ..config(SESSION)
    };
    let conn = Reliable::new(ea.clone(), clock.clone(), config).unwrap();
    (conn, ea, clock)
}

fn settle(conn: &mut Conn) -> Result<(), Failure> {
    for _ in 0..64 {
        if let Poll::Ready(result) = flush(conn) {
            return result;
        }
    }
    panic!("flush did not settle");
}

fn data_frames(endpoint: &Endpoint) -> usize {
    count(&endpoint.written_frames(), |w| {
        matches!(w, Wire::Data { .. })
    })
}

fn cx_of(waker: &Waker) -> Context<'_> {
    Context::from_waker(waker)
}

#[test]
fn reader_is_woken_by_the_progress_task() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (p_flag, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    ea.inject_incoming(&data_wire(SESSION, 0, &[1, 2, 3]));
    assert_eq!((r_flag.count(), p_flag.count()), (0, 1));
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    assert!(r_flag.count() >= 1);
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Ok(3))
    ));
    assert_eq!(&buf[..3], &[1, 2, 3]);
}

#[test]
fn writer_and_flush_waiters_are_woken_by_the_progress_task() {
    let (ea, _eb) = link();
    let clock = FakeClock::latest_waker_only();
    let mut a: Conn = Reliable::new(ea.clone(), clock, config(SESSION)).unwrap();
    let (w_flag, w) = Flag::new();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_write(&mut cx_of(&w), &[2]).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert_eq!((w_flag.count(), f_flag.count(), p_flag.count()), (0, 0, 1));
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    assert!(w_flag.count() >= 1);
    assert!(f_flag.count() >= 1);
    assert!(matches!(a.poll_flush(&mut cx_of(&f)), Poll::Ready(Ok(()))));
    assert!(matches!(
        a.poll_write(&mut cx_of(&w), &[2]),
        Poll::Ready(Ok(1))
    ));
}

#[test]
fn flush_waiter_is_woken_when_early_ack_precedes_the_end_of_output() {
    let (mut a, ea, _) = solo(RETRIES);
    let (w_flag, w) = Flag::new();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    ea.limit_writes(Some(9));
    assert!(matches!(write(&mut a, &[1, 2, 3]), Poll::Ready(Ok(3))));
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    assert!(a.poll_write(&mut cx_of(&w), &[7]).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    let before = (w_flag.count(), f_flag.count(), p_flag.count());
    ea.limit_writes(None);
    assert_eq!(p_flag.count(), before.2 + 1);
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    assert!(w_flag.count() > before.0);
    assert!(f_flag.count() > before.1);
    assert!(matches!(flush(&mut a), Poll::Ready(Ok(()))));
    let sent = ea.written_frames();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        parse_wire(&sent[0]),
        Some(Wire::Data {
            session: SESSION,
            seq: 0,
            payload: vec![1, 2, 3],
        })
    );
    assert!(matches!(write(&mut a, &[7]), Poll::Ready(Ok(1))));
}

#[test]
fn eof_wakes_pending_operations() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (w_flag, w) = Flag::new();
    let (f_flag, f) = Flag::new();
    let (_, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_write(&mut cx_of(&w), &[2]).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    ea.close_incoming();
    assert!(matches!(
        a.poll_progress(&mut cx_of(&p)),
        Poll::Ready(ConnectionError::UnexpectedEof)
    ));
    assert!(r_flag.count() >= 1 && w_flag.count() >= 1 && f_flag.count() >= 1);
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
    assert!(matches!(
        a.poll_write(&mut cx_of(&w), &[2]),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
    assert!(matches!(
        a.poll_flush(&mut cx_of(&f)),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
}

#[test]
fn transport_error_wakes_pending_operations() {
    let error = MockError("boom");
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (w_flag, w) = Flag::new();
    let (f_flag, f) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_write(&mut cx_of(&w), &[2]).is_pending());
    ea.fail_next_read(error);
    assert!(matches!(
        a.poll_flush(&mut cx_of(&f)),
        Poll::Ready(Err(ConnectionError::Transport(e))) if e == error
    ));
    assert!(r_flag.count() >= 1 && w_flag.count() >= 1);
    assert_eq!(f_flag.count(), 0);
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
    assert!(matches!(
        a.poll_write(&mut cx_of(&w), &[2]),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
}

#[test]
fn timeout_wakes_pending_operations() {
    let (ea, _eb) = link();
    let clock = FakeClock::latest_waker_only();
    let mut a: Conn = Reliable::new(
        ea,
        clock.clone(),
        Config {
            max_retries: 0,
            ..config(SESSION)
        },
    )
    .unwrap();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    clock.advance(RTO);
    assert_eq!((f_flag.count(), p_flag.count()), (0, 1));
    assert!(matches!(
        a.poll_progress(&mut cx_of(&p)),
        Poll::Ready(ConnectionError::Timeout)
    ));
    assert!(f_flag.count() >= 1);
    assert!(matches!(
        a.poll_flush(&mut cx_of(&f)),
        Poll::Ready(Err(ConnectionError::Failed))
    ));
}

#[test]
fn completed_write_does_not_steal_the_readers_notification() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (x_flag, x) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(matches!(
        a.poll_write(&mut cx_of(&x), &[1]),
        Poll::Ready(Ok(1))
    ));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    let before = r_flag.count();
    ea.inject_incoming(&data_wire(SESSION, 0, &[4, 5]));
    assert!(r_flag.count() > before);
    assert_eq!(x_flag.count(), 0);
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Ok(2))
    ));
}

#[test]
fn repolling_replaces_the_retained_waker() {
    let (mut a, ea, _) = solo(RETRIES);
    let (old_flag, old) = Flag::new();
    let (new_flag, new) = Flag::new();
    let (_, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&old), &mut buf).is_pending());
    assert!(a.poll_read(&mut cx_of(&new), &mut buf).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    ea.inject_incoming(&data_wire(SESSION, 0, &[1]));
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    assert_eq!(old_flag.count(), 0);
    assert_eq!(new_flag.count(), 1);
}

#[test]
fn quiescent_connection_does_not_wake_itself() {
    let (mut a, _ea, clock) = solo(RETRIES);
    let (flag, waker) = Flag::new();
    let mut buf = [0u8; 8];
    for _ in 0..20 {
        clock.advance(TICK);
        assert!(a.poll_progress(&mut cx_of(&waker)).is_pending());
        assert!(a.poll_read(&mut cx_of(&waker), &mut buf).is_pending());
    }
    assert_eq!(flag.count(), 0);
    assert!(matches!(
        a.poll_write(&mut cx_of(&waker), &[1]),
        Poll::Ready(Ok(1))
    ));
    let settled = flag.count();
    for _ in 0..20 {
        assert!(a.poll_flush(&mut cx_of(&waker)).is_pending());
        assert!(a.poll_progress(&mut cx_of(&waker)).is_pending());
        assert!(a.poll_read(&mut cx_of(&waker), &mut buf).is_pending());
    }
    assert_eq!(flag.count(), settled);
}

#[test]
fn acknowledgment_delivered_byte_by_byte_at_the_deadline_is_honored() {
    let (mut a, ea, clock) = solo(0);
    let (flag, waker) = Flag::new();
    ea.set_read_chunk(1);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    let ack = ack_wire(SESSION, 0);
    assert!(ack.len() > STEP_BUDGET);
    ea.inject_incoming(&ack);
    clock.advance(RTO);
    assert!(a.poll_flush(&mut cx_of(&waker)).is_pending());
    assert!(flag.count() >= 1);
    assert!(matches!(settle(&mut a), Ok(())));
    assert_eq!(data_frames(&ea), 1);
}

#[test]
fn acknowledgment_at_the_final_deadline_after_a_retransmission_is_honored() {
    let (mut a, ea, clock) = solo(1);
    ea.set_read_chunk(1);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    clock.advance(RTO);
    assert!(flush(&mut a).is_pending());
    assert_eq!(data_frames(&ea), 2);
    ea.inject_incoming(&ack_wire(SESSION, 0));
    clock.advance(RTO);
    assert!(matches!(settle(&mut a), Ok(())));
    assert_eq!(data_frames(&ea), 2);
}

#[test]
fn padding_and_malformed_frames_before_the_acknowledgment_are_skipped() {
    let (mut a, ea, clock) = solo(0);
    ea.set_read_chunk(1);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    let mut input = vec![0u8; 3];
    input.extend_from_slice(&[0x55; 40]);
    input.push(0);
    input.extend_from_slice(&ack_wire(SESSION, 0));
    ea.inject_incoming(&input);
    clock.advance(RTO);
    assert!(matches!(settle(&mut a), Ok(())));
}

#[test]
fn continuous_junk_cannot_postpone_the_timeout() {
    let clock = FakeClock::default();
    let mut conn: Reliable<Flood, FakeClock, N> = Reliable::new(
        Flood { reads: 0 },
        clock.clone(),
        Config {
            max_retries: 0,
            ..config(SESSION)
        },
    )
    .unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(conn.poll_write(&mut cx, &[1]), Poll::Ready(Ok(1))));
    let mut polls = 0;
    let result = loop {
        clock.advance(1);
        polls += 1;
        assert!(polls <= RTO + 10, "timeout postponed");
        if let Poll::Ready(result) = conn.poll_flush(&mut cx) {
            break result;
        }
    };
    assert!(matches!(result, Err(ConnectionError::Timeout)));
}

#[test]
fn late_acknowledgment_cancels_a_queued_retransmission() {
    let (mut a, ea, clock) = solo(RETRIES);
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    ea.limit_writes(Some(2));
    ea.inject_incoming(&data_wire(SESSION, 0, &[9]));
    assert!(progress(&mut a).is_pending());
    clock.advance(RTO);
    assert!(progress(&mut a).is_pending());
    assert_eq!(data_frames(&ea), 1);
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert!(progress(&mut a).is_pending());
    ea.limit_writes(None);
    assert!(matches!(settle(&mut a), Ok(())));
    assert_eq!(data_frames(&ea), 1);
    let sent = ea.written_frames();
    assert_eq!(count(&sent, |w| matches!(w, Wire::Ack { seq: 0, .. })), 1);
    clock.advance(10 * RTO);
    assert!(progress(&mut a).is_pending());
    assert_eq!(data_frames(&ea), 1);
}

type Slow = Reliable<Trickle, FakeClock, N>;

fn slow() -> (Slow, Trickle) {
    let trickle = Trickle::default();
    let conn = Reliable::new(trickle.clone(), FakeClock::default(), config(SESSION)).unwrap();
    (conn, trickle)
}

#[test]
fn bounded_output_preserves_the_unwritten_suffix() {
    let (mut conn, trickle) = slow();
    let (flag, waker) = Flag::new();
    let mut cx = cx_of(&waker);
    let payload = pattern(Slow::MAX_PAYLOAD);
    let frame = data_wire(SESSION, 0, &payload);
    assert!(frame.len() > STEP_BUDGET);
    assert!(matches!(
        conn.poll_write(&mut cx, &payload),
        Poll::Ready(Ok(len)) if len == payload.len()
    ));
    assert!(trickle.calls() <= STEP_BUDGET);
    let written = trickle.written();
    assert!(!written.is_empty() && written.len() < frame.len());
    assert_eq!(written, frame[..written.len()]);
    assert!(flag.count() >= 1);
    let mut polls = 1;
    while trickle.written().len() < frame.len() {
        let (calls, wakes) = (trickle.calls(), flag.count());
        assert!(conn.poll_progress(&mut cx).is_pending());
        assert!(trickle.calls() - calls <= STEP_BUDGET);
        let written = trickle.written();
        assert_eq!(written, frame[..written.len()]);
        if written.len() < frame.len() {
            assert!(flag.count() > wakes);
        }
        polls += 1;
        assert!(polls < 32);
    }
    assert_eq!(trickle.written(), frame);
}

#[test]
fn acknowledgments_never_interleave_with_a_trickled_frame() {
    let (mut conn, trickle) = slow();
    let mut cx = Context::from_waker(Waker::noop());
    let mut expected = Vec::new();
    let mut buf = [0u8; 8];
    for seq in 0..3u32 {
        let payload: Vec<u8> = pattern(Slow::MAX_PAYLOAD)
            .into_iter()
            .map(|byte| byte ^ seq as u8)
            .collect();
        if seq > 0 {
            trickle.inject(&ack_wire(SESSION, seq - 1));
        }
        let calls = trickle.calls();
        assert!(matches!(
            conn.poll_write(&mut cx, &payload),
            Poll::Ready(Ok(len)) if len == payload.len()
        ));
        assert!(trickle.calls() - calls <= STEP_BUDGET);
        trickle.inject(&data_wire(SESSION, seq, &[seq as u8]));
        expected.extend(data_wire(SESSION, seq, &payload));
        expected.extend(ack_wire(SESSION, seq));
        let mut polls = 0;
        while trickle.written().len() < expected.len() {
            let calls = trickle.calls();
            assert!(conn.poll_progress(&mut cx).is_pending());
            assert!(trickle.calls() - calls <= STEP_BUDGET);
            let written = trickle.written();
            assert_eq!(written, expected[..written.len()]);
            polls += 1;
            assert!(polls < 64);
        }
        assert!(matches!(
            conn.poll_read(&mut cx, &mut buf),
            Poll::Ready(Ok(1))
        ));
        assert_eq!(buf[0], seq as u8);
    }
    assert_eq!(trickle.written(), expected);
    assert!(expected.len() > 2 * N);
}

#[test]
fn cancellation_without_the_handoff_leaves_the_reader_unwoken() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (p_flag, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    ea.inject_incoming(&data_wire(SESSION, 0, &[1, 2, 3]));
    assert_eq!((r_flag.count(), p_flag.count()), (0, 1));
}

#[test]
fn cancellation_hands_the_reader_a_fresh_registration() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (p_flag, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Progress);
    assert_eq!((r_flag.count(), p_flag.count()), (1, 0));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    ea.inject_incoming(&data_wire(SESSION, 0, &[1, 2, 3]));
    assert_eq!((r_flag.count(), p_flag.count()), (2, 0));
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Ok(3))
    ));
    assert_eq!(&buf[..3], &[1, 2, 3]);
    assert_eq!(p_flag.count(), 0);
}

#[test]
fn cancellation_lets_surviving_writer_and_flusher_complete() {
    let (ea, _eb) = link();
    let clock = FakeClock::latest_waker_only();
    let mut a: Conn = Reliable::new(ea.clone(), clock, config(SESSION)).unwrap();
    let (w_flag, w) = Flag::new();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_write(&mut cx_of(&w), &[2]).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Progress);
    assert_eq!((w_flag.count(), f_flag.count(), p_flag.count()), (1, 1, 0));
    assert!(a.poll_write(&mut cx_of(&w), &[2]).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert_eq!((w_flag.count(), f_flag.count(), p_flag.count()), (1, 2, 0));
    assert!(matches!(a.poll_flush(&mut cx_of(&f)), Poll::Ready(Ok(()))));
    assert_eq!(w_flag.count(), 2);
    assert!(matches!(
        a.poll_write(&mut cx_of(&w), &[2]),
        Poll::Ready(Ok(1))
    ));
    assert_eq!(p_flag.count(), 0);
}

#[test]
fn cancellation_lets_the_flusher_reclaim_the_timer_registration() {
    let (ea, _eb) = link();
    let clock = FakeClock::latest_waker_only();
    let mut a: Conn = Reliable::new(
        ea.clone(),
        clock.clone(),
        Config {
            max_retries: 1,
            ..config(SESSION)
        },
    )
    .unwrap();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Progress);
    assert_eq!((f_flag.count(), p_flag.count()), (1, 0));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    clock.advance(RTO);
    assert_eq!((f_flag.count(), p_flag.count()), (2, 0));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert_eq!(data_frames(&ea), 2);
    clock.advance(RTO);
    assert_eq!((f_flag.count(), p_flag.count()), (3, 0));
    assert!(matches!(
        a.poll_flush(&mut cx_of(&f)),
        Poll::Ready(Err(ConnectionError::Timeout))
    ));
}

#[test]
fn cancellation_resumes_blocked_output_without_loss_or_repeat() {
    let (mut a, ea, _) = solo(RETRIES);
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    let payload = pattern(Conn::MAX_PAYLOAD);
    ea.limit_writes(Some(5));
    assert!(matches!(
        write(&mut a, &payload),
        Poll::Ready(Ok(len)) if len == payload.len()
    ));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Progress);
    assert_eq!((f_flag.count(), p_flag.count()), (1, 0));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    ea.limit_writes(None);
    assert_eq!((f_flag.count(), p_flag.count()), (2, 0));
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    let sent = ea.written_frames();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        parse_wire(&sent[0]),
        Some(Wire::Data {
            session: SESSION,
            seq: 0,
            payload,
        })
    );
    ea.inject_incoming(&ack_wire(SESSION, 0));
    assert!(matches!(a.poll_flush(&mut cx_of(&f)), Poll::Ready(Ok(()))));
    assert_eq!(data_frames(&ea), 1);
}

#[test]
fn cancellation_without_stored_waiters_does_nothing() {
    let (mut a, ea, _) = solo(RETRIES);
    for operation in [
        PendingOperation::Read,
        PendingOperation::Write,
        PendingOperation::Flush,
        PendingOperation::Progress,
    ] {
        a.cancel_pending(operation);
    }
    let (flag, waker) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_progress(&mut cx_of(&waker)).is_pending());
    assert!(a.poll_read(&mut cx_of(&waker), &mut buf).is_pending());
    assert_eq!(flag.count(), 0);
    assert!(ea.written_frames().is_empty());
}

#[test]
fn cancellation_clears_the_canceled_waker() {
    let (mut a, ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (p_flag, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Read);
    assert_eq!((r_flag.count(), p_flag.count()), (0, 1));
    ea.inject_incoming(&data_wire(SESSION, 0, &[1]));
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Write);
    assert_eq!(r_flag.count(), 0);
    assert!(matches!(
        a.poll_read(&mut cx_of(&r), &mut buf),
        Poll::Ready(Ok(1))
    ));
}

#[test]
fn cancellation_wakes_each_waiter_once() {
    let (mut a, _ea, _) = solo(RETRIES);
    let (r_flag, r) = Flag::new();
    let (f_flag, f) = Flag::new();
    let (p_flag, p) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(matches!(write(&mut a, &[1]), Poll::Ready(Ok(1))));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    assert!(a.poll_flush(&mut cx_of(&f)).is_pending());
    assert!(a.poll_progress(&mut cx_of(&p)).is_pending());
    a.cancel_pending(PendingOperation::Progress);
    a.cancel_pending(PendingOperation::Progress);
    a.cancel_pending(PendingOperation::Write);
    assert_eq!((r_flag.count(), f_flag.count(), p_flag.count()), (1, 1, 0));
    assert!(a.poll_read(&mut cx_of(&r), &mut buf).is_pending());
    a.cancel_pending(PendingOperation::Flush);
    assert_eq!((r_flag.count(), f_flag.count(), p_flag.count()), (2, 1, 0));
}

#[test]
fn cancellation_keeps_protocol_state() {
    let (mut a, ea, clock) = solo(1);
    let (_, w) = Flag::new();
    let mut buf = [0u8; 8];
    assert!(matches!(write(&mut a, &[1, 2, 3]), Poll::Ready(Ok(3))));
    ea.inject_incoming(&data_wire(SESSION, 0, &[7, 8]));
    ea.limit_writes(Some(0));
    assert!(progress(&mut a).is_pending());
    assert!(a.poll_flush(&mut cx_of(&w)).is_pending());
    let cancel_all = |a: &mut Conn| {
        for operation in [
            PendingOperation::Read,
            PendingOperation::Write,
            PendingOperation::Flush,
            PendingOperation::Progress,
        ] {
            a.cancel_pending(operation);
        }
    };
    cancel_all(&mut a);
    clock.advance(RTO);
    ea.limit_writes(None);
    assert!(progress(&mut a).is_pending());
    assert_eq!(data_frames(&ea), 2);
    let sent = ea.written_frames();
    assert_eq!(count(&sent, |w| matches!(w, Wire::Ack { seq: 0, .. })), 1);
    cancel_all(&mut a);
    assert!(matches!(read(&mut a, &mut buf), Poll::Ready(Ok(2))));
    assert_eq!(&buf[..2], &[7, 8]);
    clock.advance(RTO);
    assert!(matches!(
        progress(&mut a),
        Poll::Ready(ConnectionError::Timeout)
    ));
}
