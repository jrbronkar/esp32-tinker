use core::mem;
use core::task::{Context, Poll, Waker};

use crate::error::{ConfigError, ConnectionError};
use crate::protocol::{self, ACK_FRAME_LEN, Packet};
use crate::sync;
use crate::timer::Timer;
use crate::transport::Transport;

pub(crate) const STEP_BUDGET: usize = 16;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
/// Parameters of a [`Reliable`] connection.
///
/// Create one with [`Config::new`] and override fields as needed.
pub struct Config {
    /// Identifier of the current session.
    ///
    /// Both peers must use the same value, and it should be fresh for each
    /// session. Packets from a different session are ignored. This is not
    /// authentication.
    pub session: u64,

    /// Time to wait for an acknowledgment before retransmitting, in
    /// [`Timer`] ticks.
    ///
    /// The wait begins once the packet has been written and the underlying
    /// transport flushed.
    pub retransmit_timeout: u64,

    /// Number of retransmissions attempted after the initial transmission.
    ///
    /// When the timeout elapses with none left, the connection fails with
    /// [`ConnectionError::Timeout`]. Zero fails at the first timeout.
    pub max_retries: u32,
}

impl Config {
    /// Returns a configuration for `session` with a retransmission timeout
    /// of 1000 ticks and 8 retries.
    pub const fn new(session: u64) -> Self {
        Self {
            session,
            retransmit_timeout: 1_000,
            max_retries: 8,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tx {
    Idle,
    Queued,
    Sending,
    Waiting,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Ack,
    Data,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Out {
    Idle,
    Writing(Frame, usize),
    Flushing(Frame),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Blocked,
    Progress,
    Yield,
}

struct Budget(usize);

impl Budget {
    const fn new() -> Self {
        Self(STEP_BUDGET)
    }

    fn spend(&mut self) -> bool {
        match self.0.checked_sub(1) {
            Some(left) => {
                self.0 = left;
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
/// An operation of a [`Reliable`] connection that can be pending.
///
/// Identifies the operation abandoned in
/// [`Reliable::cancel_pending`]. The connection stores one waiter per
/// operation.
pub enum PendingOperation {
    /// [`Transport::poll_read`].
    Read,
    /// [`Transport::poll_write`].
    Write,
    /// [`Transport::poll_flush`].
    Flush,
    /// [`Reliable::poll_progress`].
    Progress,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Write,
    Flush,
    Progress,
}

const OPS: [Op; 4] = [Op::Read, Op::Write, Op::Flush, Op::Progress];

#[derive(Default)]
struct Waiters([Option<Waker>; 4]);

impl Waiters {
    fn slot(&mut self, op: Op) -> &mut Option<Waker> {
        &mut self.0[op as usize]
    }

    fn register(&mut self, op: Op, waker: &Waker) {
        let slot = self.slot(op);
        match slot {
            Some(held) if held.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
    }

    fn clear(&mut self, op: Op) {
        *self.slot(op) = None;
    }

    fn wake(&mut self, op: Op) {
        if let Some(waker) = self.slot(op).take() {
            waker.wake();
        }
    }
}

enum Health<E> {
    Ok,
    Failed(ConnectionError<E>),
    Reported,
}

/// A reliable, ordered byte stream over an unreliable [`Transport`].
///
/// Wraps the link `T` and the clock `C`, and implements [`Transport`] itself.
/// Application bytes are divided into packets, framed with COBS, protected by
/// a checksum, acknowledged and retransmitted. See the [crate
/// documentation](crate) for the protocol, semantics and limitations.
///
/// # Capacity
///
/// `N` is the size in bytes of each internal frame buffer, which bounds a
/// complete encoded packet including both delimiters. The largest payload per
/// packet is [`MAX_PAYLOAD`](Self::MAX_PAYLOAD), which accounts for the
/// 17 bytes of header and checksum and for COBS overhead; with the default
/// `N` of 256 it is 236. [`new`](Self::new) rejects capacities too small for
/// one byte of payload (any `N` below 21). The connection holds four buffers
/// of `N` bytes, plus a small ACK frame buffer.
///
/// # Driving the connection
///
/// The connection makes progress only while polled. Each poll method drives
/// both directions: it finishes partially written frames, sends pending
/// acknowledgments between frames, processes input, checks the retransmission
/// deadline and starts retransmissions. Call
/// [`poll_progress`](Self::poll_progress) when no read, write or flush is
/// outstanding.
///
/// # Cancellation
///
/// Dropping a future that awaits a poll method rolls back no protocol state
/// and corrupts no accepted data. It does not, however, notify the
/// connection. When tasks poll the operations separately, the transport and
/// timer wake only the task that polled them last, so dropping that future
/// can leave the others without a wakeup. An adapter that supports canceling
/// pending operations must call
/// [`cancel_pending`](Self::cancel_pending) for the dropped operation. The
/// connection does not detect dropped futures.
///
/// # Failure
///
/// After a transport error, EOF, retry exhaustion or sequence exhaustion, the
/// connection is permanently failed. The first poll to observe the failure
/// returns the specific [`ConnectionError`]; later polls return
/// [`ConnectionError::Failed`]. Payload already received can still be read
/// first. Use [`into_parts`](Self::into_parts) to recover the transport and
/// timer.
pub struct Reliable<T: Transport, C: Timer, const N: usize = 256> {
    transport: T,
    timer: C,
    config: Config,
    health: Health<T::Error>,
    tx: Tx,
    tx_frame: [u8; N],
    tx_len: usize,
    tx_next: u64,
    tx_deadline: u64,
    tx_retries: u32,
    final_input: Option<usize>,
    out: Out,
    ack_pending: Option<u32>,
    ack_frame: [u8; ACK_FRAME_LEN],
    ack_len: usize,
    rx_buf: [u8; N],
    rx_len: usize,
    rx_discarding: bool,
    rx_payload: [u8; N],
    rx_start: usize,
    rx_end: usize,
    rx_next: u64,
    scratch: [u8; N],
    waiters: Waiters,
}

impl<T: Transport, C: Timer, const N: usize> Reliable<T, C, N> {
    /// The largest number of application bytes carried by one packet.
    ///
    /// A single `poll_write` accepts at most this many bytes.
    pub const MAX_PAYLOAD: usize = protocol::max_payload(N);

    /// Bytes read after a deadline expires while looking for a late
    /// acknowledgment: the tail of a partially received frame plus one ACK.
    const FINAL_INPUT: usize = N + ACK_FRAME_LEN;

    /// Creates a connection over `transport`, using `timer` for deadlines.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::BufferTooSmall`] if `N` cannot hold a packet
    /// carrying at least one payload byte.
    pub fn new(transport: T, timer: C, config: Config) -> Result<Self, ConfigError> {
        if Self::MAX_PAYLOAD == 0 {
            return Err(ConfigError::BufferTooSmall);
        }
        Ok(Self {
            transport,
            timer,
            config,
            health: Health::Ok,
            tx: Tx::Idle,
            tx_frame: [0; N],
            tx_len: 0,
            tx_next: 0,
            tx_deadline: 0,
            tx_retries: 0,
            final_input: None,
            out: Out::Idle,
            ack_pending: None,
            ack_frame: [0; ACK_FRAME_LEN],
            ack_len: 0,
            rx_buf: [0; N],
            rx_len: 0,
            rx_discarding: false,
            rx_payload: [0; N],
            rx_start: 0,
            rx_end: 0,
            rx_next: 0,
            scratch: [0; N],
            waiters: Waiters::default(),
        })
    }

    /// Consumes the connection and returns the transport and timer.
    ///
    /// Buffered data and protocol state are discarded.
    pub fn into_parts(self) -> (T, C) {
        (self.transport, self.timer)
    }

    /// Drives the connection without reading, writing or flushing.
    ///
    /// Use it to keep acknowledgments, retransmissions and incoming data
    /// flowing while the application has no operation pending. Incoming
    /// payload is retained for later reads, and further data is not
    /// acknowledged until the application reads it.
    ///
    /// Returns `Pending` for as long as the connection is healthy, with
    /// wakeups registered. Returns `Ready` with the error once the
    /// connection has failed.
    pub fn poll_progress(&mut self, cx: &mut Context<'_>) -> Poll<ConnectionError<T::Error>> {
        self.drive(cx, &mut Budget::new());
        let poll = match self.failure() {
            Some(error) => Poll::Ready(error),
            None => Poll::Pending,
        };
        self.finish(Op::Progress, cx, poll)
    }

    /// Abandons a pending `operation` and hands its wakeups to the others.
    ///
    /// The underlying [`Transport`] and [`Timer`] remember only the waker of
    /// their latest poller. If that poller is dropped while pending, the
    /// registration is lost and the connection cannot detect it, so the
    /// surviving operations may never be woken. This method clears the stored
    /// waiter of `operation` and wakes and removes every other stored waiter.
    /// Each woken task polls again and re-registers with the transport and
    /// timer.
    ///
    /// Protocol state is unchanged: an accepted packet, a partially written
    /// frame, retained incoming payload, pending acknowledgments and
    /// retransmission deadlines and retry counts all survive. Nothing is
    /// sent, and no task is woken if no waiter is stored. Waiters are woken
    /// once per call, so calling it repeatedly does not wake a task again
    /// until it has polled and registered again.
    ///
    /// # Serialization
    ///
    /// Call it with exclusive access to the connection, after the last poll
    /// of the abandoned operation and before the operation is forgotten. The
    /// connection stores one waiter per operation, and does not distinguish
    /// between futures awaiting the same one. Canceling an older future of an
    /// operation while a newer one is pending clears the newer one's waiter,
    /// so the adapter must prevent that or track the futures itself.
    pub fn cancel_pending(&mut self, operation: PendingOperation) {
        let canceled = match operation {
            PendingOperation::Read => Op::Read,
            PendingOperation::Write => Op::Write,
            PendingOperation::Flush => Op::Flush,
            PendingOperation::Progress => Op::Progress,
        };
        self.waiters.clear(canceled);
        for op in OPS {
            if op != canceled {
                self.waiters.wake(op);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_sequences(&mut self, tx_next: u64, rx_next: u64) {
        self.tx_next = tx_next;
        self.rx_next = rx_next;
    }

    fn fail(&mut self, error: ConnectionError<T::Error>) {
        self.final_input = None;
        if matches!(self.health, Health::Ok) {
            self.health = Health::Failed(error);
        }
    }

    fn failure(&mut self) -> Option<ConnectionError<T::Error>> {
        match mem::replace(&mut self.health, Health::Reported) {
            Health::Ok => {
                self.health = Health::Ok;
                None
            }
            Health::Failed(error) => Some(error),
            Health::Reported => Some(ConnectionError::Failed),
        }
    }

    fn data_out_busy(&self) -> bool {
        matches!(
            self.out,
            Out::Writing(Frame::Data, _) | Out::Flushing(Frame::Data)
        )
    }

    fn queue_packet(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, ConnectionError<T::Error>>> {
        let mut budget = Budget::new();
        let yielded = self.drive(cx, &mut budget) == Work::Yield;
        let busy = !self.write_ready();
        if !busy && self.tx_next > u64::from(u32::MAX) {
            self.fail(ConnectionError::SequenceExhausted);
        }
        if let Some(error) = self.failure() {
            return Poll::Ready(Err(error));
        }
        if busy {
            return Poll::Pending;
        }
        let len = buf.len().min(Self::MAX_PAYLOAD);
        self.tx_len = protocol::encode_data(
            self.config.session,
            self.tx_next as u32,
            &buf[..len],
            &mut self.scratch,
            &mut self.tx_frame,
        );
        self.tx_next += 1;
        self.tx_retries = 0;
        self.tx = Tx::Queued;
        if !yielded {
            self.drive(cx, &mut budget);
        }
        Poll::Ready(Ok(len))
    }

    fn write_ready(&self) -> bool {
        self.tx == Tx::Idle && !self.data_out_busy()
    }

    fn flush_ready(&self) -> bool {
        self.tx == Tx::Idle && self.out == Out::Idle && self.ack_pending.is_none()
    }

    fn finish<R>(&mut self, op: Op, cx: &mut Context<'_>, poll: Poll<R>) -> Poll<R> {
        let completed = poll.is_ready();
        if completed {
            self.waiters.clear(op);
        } else {
            self.waiters.register(op, cx.waker());
        }
        self.notify(op, completed);
        poll
    }

    /// Wakes the retained waiters of the other operations whose condition now
    /// holds. A completed operation, or a failure, wakes all of them: the
    /// transport and timer remember only their latest poller, which may be a
    /// task that is no longer waiting.
    fn notify(&mut self, done: Op, completed: bool) {
        let everyone = completed || !matches!(self.health, Health::Ok);
        for op in OPS {
            if op == done {
                continue;
            }
            let due = everyone
                || match op {
                    Op::Read => self.rx_start < self.rx_end,
                    Op::Write => self.write_ready(),
                    Op::Flush => self.flush_ready(),
                    Op::Progress => false,
                };
            if due {
                self.waiters.wake(op);
            }
        }
    }

    fn drive(&mut self, cx: &mut Context<'_>, budget: &mut Budget) -> Work {
        if !matches!(self.health, Health::Ok) {
            return Work::Blocked;
        }
        loop {
            match self.step(cx, budget) {
                Ok(Work::Progress) => {}
                Ok(Work::Blocked) => return Work::Blocked,
                Ok(Work::Yield) => {
                    cx.waker().wake_by_ref();
                    return Work::Yield;
                }
                Err(error) => {
                    self.fail(error);
                    return Work::Blocked;
                }
            }
        }
    }

    fn step(
        &mut self,
        cx: &mut Context<'_>,
        budget: &mut Budget,
    ) -> Result<Work, ConnectionError<T::Error>> {
        let output = self.poll_output(cx, budget)?;
        let input = self.poll_input(cx, budget)?;
        let timer = self.poll_timer(cx, input)?;
        Ok(output.max(input).max(timer))
    }

    fn poll_output(
        &mut self,
        cx: &mut Context<'_>,
        budget: &mut Budget,
    ) -> Result<Work, ConnectionError<T::Error>> {
        if self.out == Out::Idle {
            if let Some(seq) = self.ack_pending.take() {
                self.ack_len = protocol::encode_ack(self.config.session, seq, &mut self.ack_frame);
                self.out = Out::Writing(Frame::Ack, 0);
            } else if self.tx == Tx::Queued {
                self.tx = Tx::Sending;
                self.out = Out::Writing(Frame::Data, 0);
            } else {
                return Ok(Work::Blocked);
            }
        }
        if !budget.spend() {
            return Ok(Work::Yield);
        }
        match self.out {
            Out::Idle => Ok(Work::Blocked),
            Out::Writing(frame, offset) => {
                let (pending, len) = match frame {
                    Frame::Ack => (&self.ack_frame[offset..self.ack_len], self.ack_len),
                    Frame::Data => (&self.tx_frame[offset..self.tx_len], self.tx_len),
                };
                let written = match self.transport.poll_write(cx, pending) {
                    Poll::Pending => return Ok(Work::Blocked),
                    Poll::Ready(Err(error)) => return Err(ConnectionError::Transport(error)),
                    Poll::Ready(Ok(0)) => return Err(ConnectionError::WriteZero),
                    Poll::Ready(Ok(written)) => written.min(pending.len()),
                };
                let offset = offset + written;
                self.out = if offset == len {
                    Out::Flushing(frame)
                } else {
                    Out::Writing(frame, offset)
                };
                Ok(Work::Progress)
            }
            Out::Flushing(frame) => match self.transport.poll_flush(cx) {
                Poll::Pending => Ok(Work::Blocked),
                Poll::Ready(Err(error)) => Err(ConnectionError::Transport(error)),
                Poll::Ready(Ok(())) => {
                    if frame == Frame::Data && self.tx == Tx::Sending {
                        self.tx = Tx::Waiting;
                        self.final_input = None;
                        self.tx_deadline = self
                            .timer
                            .now()
                            .saturating_add(self.config.retransmit_timeout);
                    }
                    self.out = Out::Idle;
                    Ok(Work::Progress)
                }
            },
        }
    }

    fn poll_input(
        &mut self,
        cx: &mut Context<'_>,
        budget: &mut Budget,
    ) -> Result<Work, ConnectionError<T::Error>> {
        if !budget.spend() {
            return Ok(Work::Yield);
        }
        let free = &mut self.rx_buf[self.rx_len..];
        let read = match self.transport.poll_read(cx, free) {
            Poll::Pending => return Ok(Work::Blocked),
            Poll::Ready(Err(error)) => return Err(ConnectionError::Transport(error)),
            Poll::Ready(Ok(0)) => return Err(ConnectionError::UnexpectedEof),
            Poll::Ready(Ok(read)) => read.min(free.len()),
        };
        self.rx_len += read;
        if let Some(left) = self.final_input.as_mut() {
            *left = left.saturating_sub(read);
        }
        self.process_input();
        Ok(Work::Progress)
    }

    /// Expiry of the deadline does not fail the packet at once: ready input
    /// gets a final allowance, since the acknowledgment may already be queued
    /// behind other bytes. The packet is retransmitted, or the connection
    /// times out, once the input blocks or the allowance is spent.
    fn poll_timer(
        &mut self,
        cx: &mut Context<'_>,
        input: Work,
    ) -> Result<Work, ConnectionError<T::Error>> {
        if self.tx != Tx::Waiting {
            return Ok(Work::Blocked);
        }
        match self.final_input {
            None => {
                if self.timer.poll_deadline(cx, self.tx_deadline).is_pending() {
                    return Ok(Work::Blocked);
                }
                self.final_input = Some(Self::FINAL_INPUT);
                Ok(Work::Progress)
            }
            Some(left) if input == Work::Blocked || left == 0 => {
                self.final_input = None;
                if self.tx_retries >= self.config.max_retries {
                    return Err(ConnectionError::Timeout);
                }
                self.tx_retries += 1;
                self.tx = Tx::Queued;
                Ok(Work::Progress)
            }
            Some(_) => Ok(Work::Blocked),
        }
    }

    fn process_input(&mut self) {
        loop {
            let Some(end) = self.rx_buf[..self.rx_len]
                .iter()
                .position(|&byte| byte == 0)
            else {
                if self.rx_discarding || self.rx_len == N {
                    self.rx_discarding = true;
                    self.rx_len = 0;
                }
                return;
            };
            if self.rx_discarding {
                self.rx_discarding = false;
            } else if end > 0 {
                self.process_frame(end);
            }
            self.rx_buf.copy_within(end + 1..self.rx_len, 0);
            self.rx_len -= end + 1;
        }
    }

    fn process_frame(&mut self, end: usize) {
        let Ok(decoded) = sync::decode_to_slice(&self.rx_buf[..=end], &mut self.scratch) else {
            return;
        };
        match protocol::parse(&self.scratch[..decoded.len]) {
            Some(Packet::Ack { session, seq }) => {
                if session == self.config.session
                    && self.tx != Tx::Idle
                    && u64::from(seq) + 1 == self.tx_next
                {
                    self.tx = Tx::Idle;
                    self.tx_retries = 0;
                    self.final_input = None;
                }
            }
            Some(Packet::Data {
                session,
                seq,
                payload,
            }) => {
                if session != self.config.session || payload.len() > Self::MAX_PAYLOAD {
                    return;
                }
                if u64::from(seq) == self.rx_next {
                    if self.rx_start == self.rx_end {
                        self.rx_payload[..payload.len()].copy_from_slice(payload);
                        self.rx_start = 0;
                        self.rx_end = payload.len();
                        self.rx_next += 1;
                        self.ack_pending = Some(seq);
                    }
                } else if self.rx_next > 0
                    && u64::from(seq) == self.rx_next - 1
                    && self.ack_pending.is_none()
                {
                    self.ack_pending = Some(seq);
                }
            }
            None => {}
        }
    }
}

impl<T: Transport, C: Timer, const N: usize> Transport for Reliable<T, C, N> {
    type Error = ConnectionError<T::Error>;

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Self::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.drive(cx, &mut Budget::new());
        let poll = if self.rx_start < self.rx_end {
            let len = buf.len().min(self.rx_end - self.rx_start);
            buf[..len].copy_from_slice(&self.rx_payload[self.rx_start..self.rx_start + len]);
            self.rx_start += len;
            Poll::Ready(Ok(len))
        } else {
            match self.failure() {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Pending,
            }
        };
        self.finish(Op::Read, cx, poll)
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Self::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let poll = self.queue_packet(cx, buf);
        self.finish(Op::Write, cx, poll)
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.drive(cx, &mut Budget::new());
        let poll = if let Some(error) = self.failure() {
            Poll::Ready(Err(error))
        } else if self.flush_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        };
        self.finish(Op::Flush, cx, poll)
    }
}
