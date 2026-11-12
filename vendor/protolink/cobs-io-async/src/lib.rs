//! # Reliable byte streams over COBS framing
//!
//! A runtime-independent, `no_std` transport for an unreliable point-to-point
//! link, such as a serial line. [`Reliable`] implements [`Transport`], a
//! poll-based byte-stream interface. It splits written bytes into packets,
//! frames them with Consistent Overhead Byte Stuffing (COBS), verifies them
//! with a checksum, acknowledges them, and retransmits lost ones. Packet
//! boundaries are internal: applications write and read bytes.
//!
//! The Cargo package is `cobs-io-async`; the Rust import name is
//! `cobs_io_async`.
//!
//! # Overview
//!
//! | Item | Role |
//! |---|---|
//! | [`Transport`] | Poll-based byte-stream trait, both for the underlying link you supply and for [`Reliable`] itself. |
//! | [`Timer`] | Monotonic clock and wakeup source you supply for retransmission deadlines. |
//! | [`Reliable`] | The connection. Wraps a [`Transport`] and a [`Timer`]. |
//! | [`Config`] | Session identifier, retransmission timeout and retry limit. |
//! | [`sync`] | In-memory COBS encoding and decoding between slices. |
//!
//! This crate provides no adapters for any runtime and spawns no tasks. You
//! implement [`Transport`] for your serial port, socket or driver, and
//! [`Timer`] for your clock, and wrap them in whatever your runtime needs.
//! Neither trait requires `Send`, `'static` or pinning.
//!
//! **Nothing happens unless the connection is polled.** Retransmissions,
//! acknowledgments and incoming data are processed only inside
//! [`Transport::poll_read`], [`Transport::poll_write`],
//! [`Transport::poll_flush`] and [`Reliable::poll_progress`]. While none of
//! these is pending, call [`Reliable::poll_progress`] from a task of your own
//! to keep the connection alive.
//!
//! # Behavior
//!
//! | Operation | Behavior |
//! |---|---|
//! | `poll_write(buf)` | Copies a nonempty prefix of `buf` into the outgoing packet and returns the number of bytes accepted, at most [`Reliable::MAX_PAYLOAD`]. Returns `Pending` while the previous packet is unacknowledged. |
//! | `poll_read(buf)` | Copies verified payload bytes into `buf`. Packet boundaries are invisible. Returns `Pending` while no payload is available. |
//! | `poll_flush()` | Completes when all accepted data is acknowledged by the peer and the underlying transport has been flushed. |
//! | Empty `buf` | Returns `Ready(Ok(0))` and creates no packet. |
//! | Corrupted or malformed frame | Discarded silently. Retransmission provides recovery. |
//! | Incoming payload slot full | New DATA is discarded without acknowledgment, so the peer retransmits it. Acknowledgments and duplicates are still processed. |
//! | Transport error, EOF, retries exhausted, sequence numbers exhausted | The connection fails permanently. See [`ConnectionError`]. |
//! | Dropping a future awaiting a poll method | Protocol state and accepted data are unaffected. Multi-task adapters must call [`Reliable::cancel_pending`] to preserve the other operations' wakeups. |
//!
//! Every poll method drives both directions, performing a bounded amount of
//! work per call. If work remains when the budget is spent, the task is woken
//! before `Pending` is returned, so an always-ready transport cannot
//! monopolize the executor.
//!
//! # Cancellation
//!
//! The connection owns all protocol state, so dropping a future never rolls
//! it back or corrupts accepted data. It does not detect the drop, however.
//! The underlying [`Transport`] and [`Timer`] wake only their latest poller,
//! so if the task that polled them last is canceled, another pending task may
//! never be woken.
//!
//! An adapter that polls operations from separate tasks must therefore:
//!
//! 1. Record which operation, as a [`PendingOperation`], returned `Pending`.
//! 2. If its future is dropped, obtain exclusive access to the
//!    [`Reliable`], serialized with polling.
//! 3. Call [`Reliable::cancel_pending`] for that operation before abandoning
//!    it.
//!
//! The call wakes each other stored waiter once, and those tasks poll again
//! and re-register. Only one waiter is stored per operation, so concurrent
//! futures for the same operation are not tracked separately, and a stale
//! future must not cancel a newer one. Without the call, the stall remains
//! possible.
//!
//! # Underlying transport requirements
//!
//! The link underneath [`Reliable`] must behave as a byte stream that may
//! lose, corrupt, insert or delete bytes, but must not reorder or delay them
//! beyond the retransmission timeout:
//!
//! - `poll_read` returning `Ok(0)` for a nonempty buffer means end of
//!   stream. Temporary lack of input must return `Pending`.
//! - `poll_write` returning `Ok(0)` for a nonempty buffer is an error.
//! - A returned `Pending` must have registered a wakeup.
//!
//! # Protocol
//!
//! Reliability is stop-and-wait in each direction independently: one
//! unacknowledged outgoing packet and one retained incoming payload. This
//! favors small, fixed memory over throughput.
//!
//! Each packet is
//!
//! ```text
//! 0 | COBS(kind | session | sequence | payload | checksum) | 0
//! ```
//!
//! with big-endian integers:
//!
//! | Field | Size | Meaning |
//! |---|---|---|
//! | `kind` | 1 | `1` for DATA, `2` for ACK. |
//! | `session` | 8 | Session identifier from [`Config::session`]. |
//! | `sequence` | 4 | DATA sequence number, starting at 0. An ACK echoes it. |
//! | `payload` | 1 or more for DATA, absent for ACK | Application bytes. The length is implied by the frame boundary. |
//! | `checksum` | 4 | CRC-32/ISO-HDLC over all preceding decoded fields. |
//!
//! A receiver accepts DATA only if the session matches, the checksum is
//! valid and the sequence is the next expected one. It acknowledges data once
//! it has *retained* it, not once the application has read it. A repeat of
//! the previous DATA is acknowledged again but never delivered twice. Any
//! other DATA is dropped. A sender releases its packet only for an ACK
//! with a matching session and sequence.
//!
//! The receive path collects bytes until a zero delimiter and decodes only
//! delimiter-terminated frames. Repeated zeros are padding. A frame longer
//! than the connection's buffer is discarded through its delimiter, and the
//! following frame is processed normally.
//!
//! # Limitations
//!
//! - Both peers must be constructed with the same session identifier, chosen
//!   by the caller and fresh for each session. It distinguishes sessions; it
//!   is not authentication.
//! - There is no handshake or reconnection. Peer restarts, and packets
//!   delayed beyond a session, are outside the failure model. Sequence
//!   numbers alone do not handle them.
//! - Sequence numbers do not wrap. After 2<sup>32</sup> packets in one
//!   direction the connection fails with
//!   [`ConnectionError::SequenceExhausted`]; start a new session.
//! - A peer that stops reading for longer than the retransmission budget
//!   (`retransmit_timeout * (max_retries + 1)`) causes the sender to fail
//!   with [`ConnectionError::Timeout`].
//! - Each accepted write becomes its own packet. Small writes are not
//!   coalesced.
//! - There is no sliding window, adaptive timeout or fragmentation metadata.
//!
//! # Example
//!
//! The stubs below stand in for a real link and clock. A real
//! [`Timer::poll_deadline`] must register `cx.waker()` when the deadline is
//! in the future.
//!
//! ```
//! # use core::convert::Infallible;
//! # use core::task::{Context, Poll, Waker};
//! use cobs_io_async::{Config, Reliable, Timer, Transport};
//!
//! # struct Link;
//! # impl Transport for Link {
//! #     type Error = Infallible;
//! #     fn poll_read(&mut self, _: &mut Context<'_>, _: &mut [u8]) -> Poll<Result<usize, Infallible>> {
//! #         Poll::Pending
//! #     }
//! #     fn poll_write(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Infallible>> {
//! #         Poll::Ready(Ok(buf.len()))
//! #     }
//! #     fn poll_flush(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
//! #         Poll::Ready(Ok(()))
//! #     }
//! # }
//! # struct Clock(u64);
//! # impl Timer for Clock {
//! #     fn now(&self) -> u64 {
//! #         self.0
//! #     }
//! #     fn poll_deadline(&mut self, _: &mut Context<'_>, deadline: u64) -> Poll<()> {
//! #         if self.0 >= deadline { Poll::Ready(()) } else { Poll::Pending }
//! #     }
//! # }
//! let mut connection: Reliable<Link, Clock> =
//!     Reliable::new(Link, Clock(0), Config::new(0xC0B5)).unwrap();
//! let mut cx = Context::from_waker(Waker::noop());
//!
//! assert!(matches!(connection.poll_write(&mut cx, b"hello"), Poll::Ready(Ok(5))));
//! // The peer has not acknowledged the packet, so the flush stays pending.
//! assert!(connection.poll_flush(&mut cx).is_pending());
//! ```
//!
//! # In-memory encoding and decoding
//!
//! The [`sync`] module exposes the underlying COBS codec between slices. It
//! needs no allocation and is independent of [`Reliable`].
//!
//! COBS transforms a payload into an encoded body containing no zero bytes,
//! so zero can delimit frames. Payload zeros are encoded as data. An empty
//! payload encodes as `[1]`, or `[0, 1, 0]` with surrounding delimiters. A
//! terminal full block of 254 nonzero payload bytes does not receive a
//! redundant trailing code byte.
//!
//! Use [`max_encoding_length`] to size a buffer for an undelimited body, and
//! reserve two more bytes for delimiters, using checked arithmetic when sizes
//! come from untrusted or potentially large lengths.
//!
//! COBS alone provides framing, not integrity or authenticity.
//!
//! # Features
//!
//! No features are enabled by default.
//!
//! | Feature | Effect |
//! |---|---|
//! | `serde` | Serialization and deserialization of [`Config`], [`ConfigError`], [`ConnectionError`], [`DecodeError`], [`SeekableError`] and [`DecodeProgress`], subject to generic parameter bounds. Live connection state is not serializable. |
//! | `defmt` | Compact diagnostic formatting for the same types. |
//!
//! The crate is `no_std` and never allocates.

#![no_std]
#![warn(missing_docs)]

#[cfg(test)]
extern crate std;

pub use codec::decode::DecodeProgress;

mod codec;
pub(crate) mod connection;
mod error;
mod protocol;
mod timer;
mod transport;

pub mod sync;

pub use connection::{Config, PendingOperation, Reliable};
pub use error::{ConfigError, ConnectionError, DecodeError, SeekableError};
pub use timer::Timer;
pub use transport::Transport;

#[cfg(test)]
mod tests;

/// Returns the maximum additional bytes needed for an undelimited encoding.
///
/// Returns `1` for empty input; otherwise returns `source_len.div_ceil(254)`.
/// The result excludes leading and trailing frame delimiters.
///
/// This is an upper bound across payloads of the given length, not necessarily
/// the actual overhead for a particular payload.
///
/// See [`max_encoding_length`] for the corresponding total buffer size.
#[inline]
pub const fn max_encoding_overhead(source_len: usize) -> usize {
    if source_len == 0 {
        return 1;
    }
    source_len.div_ceil(254)
}

/// Returns the maximum encoded body length for `source_len` payload bytes.
///
/// The result is `source_len + max_encoding_overhead(source_len)`.
/// It excludes frame delimiters. To surround the body with leading and
/// trailing zero delimiters, reserve two additional bytes, checking that
/// the addition fits.
///
/// Empty input requires one byte. For example, 254 payload bytes require at
/// most 255 encoded bytes.
///
/// # Overflow
///
/// The addition is not checked explicitly. Overflow panics when overflow
/// checks are enabled and otherwise wraps; constant evaluation rejects it.
///
/// For potentially unrepresentable sizes, use
/// `source_len.checked_add(max_encoding_overhead(source_len))` instead.
#[inline]
pub const fn max_encoding_length(source_len: usize) -> usize {
    source_len + max_encoding_overhead(source_len)
}
