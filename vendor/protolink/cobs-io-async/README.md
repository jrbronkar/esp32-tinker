[![Crates.io](https://img.shields.io/crates/v/cobs-io-async)](https://crates.io/crates/cobs-io-async)
[![docs.rs](https://img.shields.io/docsrs/cobs-io-async)](https://docs.rs/cobs-io-async)
[![ci](https://github.com/thives/cobs-io-async.rs/actions/workflows/ci.yml/badge.svg)](https://github.com/thives/cobs-io-async.rs/actions/workflows/ci.yml)

# cobs-io-async

> [!WARNING]
> This crate is in early development. The API is not yet stable and may change.

> [!CAUTION]
> This crate is not yet production-ready. It has not been widely tested and may contain bugs.

A runtime-independent, `no_std` reliable byte-stream transport for unreliable
point-to-point links, built on
[Consistent Overhead Byte Stuffing (COBS)](https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing)
framing.

`Reliable` implements a poll-based `Transport` trait. Applications write and
read bytes; the connection divides them into packets, frames and checksums
them, acknowledges them, retransmits lost ones, and delivers them in order.
Packet boundaries are invisible.

- No runtime dependency: you supply a `Transport` for your link and a `Timer`
  for your clock. The crate provides no runtime adapters and spawns no tasks.
- Fixed memory, no heap allocation, `no_std`.
- Stop-and-wait reliability in each direction, with CRC-32 integrity checking.
- Corrupted, truncated and oversized frames are discarded and recovered by
  retransmission.
- Synchronous slice-to-slice COBS encoding and decoding in `sync`.

The package name is `cobs-io-async`; its Rust import name is `cobs_io_async`.

[API documentation](https://docs.rs/cobs-io-async/latest/cobs_io_async/)

## Installation

The reliable transport is not yet in a published release. Until then, depend
on the repository:

```toml
[dependencies]
cobs-io-async = { git = "https://github.com/thives/cobs-io-async.rs" }
```

No features are enabled by default.

## Usage

Implement the two traits for your platform:

```rust
pub trait Transport {
    type Error;

    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, Self::Error>>;
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Self::Error>>;
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
}

pub trait Timer {
    fn now(&self) -> u64;
    fn poll_deadline(&mut self, cx: &mut Context<'_>, deadline: u64) -> Poll<()>;
}
```

Then wrap them. Both peers must use the same session identifier:

```rust
use cobs_io_async::{Config, Reliable};

let config = Config {
    session: 0xC0B5,
    retransmit_timeout: 200, // in Timer ticks
    max_retries: 8,
};
let mut connection: Reliable<MyLink, MyClock> =
    Reliable::new(link, clock, config).unwrap();
```

`connection` is itself a `Transport`. Wrap it in your runtime's I/O traits, or
poll it directly. Adapters are yours to write because pinning, `Send` and
executor requirements differ between runtimes.

**The connection makes progress only while polled.** Every `poll_read`,
`poll_write` and `poll_flush` drives both directions. While none is pending,
call `Reliable::poll_progress` from a task of your own so acknowledgments and
retransmissions keep flowing. It returns `Ready` only with the error that
terminated the connection.

## Behavior

| Operation | Behavior |
|---|---|
| `poll_write(buf)` | Accepts at most `Reliable::MAX_PAYLOAD` bytes of `buf` and returns the count. Returns `Pending` while the previous packet is unacknowledged. |
| `poll_read(buf)` | Copies verified payload bytes. Returns `Pending` when none are available. |
| `poll_flush()` | Completes when all accepted data is acknowledged and the underlying transport is flushed. |
| Empty buffer | Returns `Ready(Ok(0))`; creates no packet. |
| Corrupted or malformed frame | Discarded; retransmission recovers. |
| Receive slot full | New data is not acknowledged, so the peer retransmits it. |
| Transport error, EOF, exhausted retries or sequence numbers | The connection fails permanently with a `ConnectionError`. |
| Dropped future | Protocol state and accepted data are unaffected. Multi-task adapters must call `Reliable::cancel_pending` to preserve the other operations' wakeups. |

The underlying transport must report end of stream as `Ok(0)` from
`poll_read` with a nonempty buffer, and temporary lack of input as `Pending`.

Each poll method does a bounded amount of work, and wakes the task itself if
work remains, so an always-ready transport cannot monopolize an executor.

### Cancellation

Dropping a future never rolls back protocol state or corrupts accepted data,
but the connection cannot detect the drop. The underlying transport and timer
wake only their latest poller, so canceling it can leave another pending task
without a wakeup. An adapter that polls operations from separate tasks must,
when it drops a pending future:

1. obtain exclusive access to the `Reliable`, serialized with polling;
2. call `Reliable::cancel_pending` with the `PendingOperation` that returned
   `Pending`, before abandoning it.

This clears that operation's waiter and wakes each other stored waiter once;
those tasks poll again and re-register. The connection stores one waiter per
operation, so an old canceled future must not cancel a newer one for the same
operation. Without the call, the stall remains possible.

### Capacity

`Reliable<T, C, N>` takes the size `N` (default 256) of each internal frame
buffer, bounding a complete encoded packet including delimiters. The usable
payload per packet is `Reliable::MAX_PAYLOAD` (236 for the default). `N` must
be at least 21. The connection holds four buffers of `N` bytes plus a small
ACK buffer.

### Wire format

```text
0 | COBS(kind | session | sequence | payload | checksum) | 0
```

Integers are big-endian. `kind` is `1` (DATA) or `2` (ACK), `session` is 8
bytes, `sequence` is 4 bytes, DATA has at least one payload byte and ACK none,
and `checksum` is CRC-32/ISO-HDLC over the preceding fields. A receiver
acknowledges data once it has retained it, not once the application has read
it, and never delivers a retransmitted duplicate twice.

### Limitations

- Both peers must be constructed with the same session identifier, fresh for
  each session. It is not authentication.
- There is no handshake or reconnection. Peer restarts and packets delayed
  beyond a session are not handled.
- Sequence numbers do not wrap. After 2^32 packets in one direction the
  connection fails with `ConnectionError::SequenceExhausted`.
- A peer that stops reading for longer than
  `retransmit_timeout * (max_retries + 1)` makes the sender fail with
  `ConnectionError::Timeout`.
- Each accepted write becomes its own packet; there is no coalescing, sliding
  window or adaptive timeout.

## In-memory COBS

The `sync` module encodes and decodes between slices without allocation:

```rust
use cobs_io_async::{max_encoding_length, sync};

fn main() {
    let mut encoded = [0u8; max_encoding_length(3) + 2];
    let len = sync::encode_from_slice_including_sentinels(&[7, 0, 8], &mut encoded).unwrap();
    assert_eq!(&encoded[..len], &[0, 2, 7, 2, 8, 0]);

    let mut decoded = [0u8; 3];
    let frame = sync::decode_to_slice(&encoded[..len], &mut decoded).unwrap();
    assert_eq!(decoded, [7, 0, 8]);
    assert_eq!(frame.consumed, len);
}
```

An empty payload encodes as `[1]`, or `[0, 1, 0]` with surrounding delimiters.
Zeros outside an active frame are padding. Use `max_encoding_length` for an
upper bound on an undelimited body, reserve two more bytes for delimiters, and
use checked arithmetic for potentially large lengths.

`sync::decode_to_slice` also accepts a structurally complete frame at end of
input. That cannot detect truncation at a COBS block boundary, so check that
`consumed` includes a delimiter when your protocol requires one. `Reliable`
never uses that behavior.

COBS provides framing, not integrity or authenticity.

## Features

| Feature | Effect |
|---|---|
| `serde` | Serialization of `Config`, `ConfigError`, `ConnectionError`, `DecodeError`, `SeekableError` and `DecodeProgress`, subject to generic bounds. Live connection state is not serializable. |
| `defmt` | Compact diagnostic formatting for the same types. |

## Development

The manifest declares Rust 1.87 as the minimum supported version.

Project recipes use [just](https://github.com/casey/just):

```sh
just build
just test
just docs
```

`just test` requires [cargo-nextest](https://nexte.st/).
The documentation recipe requires a nightly Rust toolchain.

Tests can also be run directly with Cargo:

```sh
cargo test --no-default-features
cargo test --all-features
```

## License

Licensed under either of:

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
