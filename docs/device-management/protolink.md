# Concurrent gRPC management and OTA

Every `device-demo` build includes cleartext HTTP/2/gRPC on TCP **7780**. gRPC is
the sole OTA transport. TCP **7779** remains read-only for status, profiling and
log polling. The legacy writer, CLI `--legacy`, old manual upload harness and
optional `experimental-grpc` feature are removed. USB/BOOT/RESET is recovery.

Protolink is pinned to `62cc2c87a687ba65fc6885e7c9691cefacf2d064`; Cargo.lock pins
zerodds HTTP/2 and HPACK to 1.0.0-rc.7. The original `esp.experimental.Management`
service name remains stable. Raw Handler protobuf bodies use existing micropb
0.7, independent of Protolink's optional micropb 0.6 codec.

## API and resource limits

See [protocol](../ota/protocol.md) and `protocol/management.proto`. Status is
unary, Logs is server streaming, and Upload is bidirectional streaming. Upload
operations are Begin, ordered sector-sized Chunk, Finish, then explicit Commit
or Abort. Clients await each correlated ACK. Finish validates the actual stored
image; the complete firmware is never buffered in device RAM.

Two active streams, two fixed subscription slots and one upload lease are
allowed per connection. Each upload has one pending ACK. Receive windows are
8192 bytes per stream, the connection window is 65535 (the HTTP/2 minimum),
decoded headers are capped at 1024, serialized output at 8192 and messages at
4608. Responses are bounded to 512 bytes. Compression is disabled.

The Embassy driver splits the TCP socket into independently polled halves.
Typed lifecycle events drive watchdog, state and reboot behavior; log wording
is presentation only. External log producers register/wake the connection task. Logs use a 32-record
RAM ring, four-entry batches, boot-scoped cursors and overwritten-history flags.
Slow log readers cannot consume the upload stream's receive window.

The portable five-second upload watchdog is refreshed only by accepted upload
operations. Log traffic and HTTP/2 keepalives cannot keep an abandoned upload
holding flash. Expiry closes the connection and the driver cancels calls,
releasing the lease. Abort/cancellation/completion clears the deadline. Pending
activation uses the separate reboot path. TCP inactivity is also bounded to ten
seconds and a connection to 180 seconds; gRPC request deadlines can expire sooner.

Once Commit is attempted after verification, activation is latched before boot
metadata writes and further writes are blocked until reset. The driver gets up
to two seconds to send the ACK/trailers, then attempts a bounded TCP flush and
reboots; disconnect also triggers pending reboot. A valid Committed ACK is
success even if trailers are truncated. Lost/invalid ACKs or metadata failures
are uncertain outcomes, never automatic retries.

## Legacy feature parity

| Capability | gRPC evidence |
| --- | --- |
| Metadata, size, descriptor and version checks | Same ota-core rules plus Tonic rejection tests |
| Ordered chunks, IDs and inactive target | Handler/CLI tests check order, request correlation and boot inventory |
| Flash readback, SHA-256 and ESP image validation | Real updater in tests and full images verified on hardware |
| Verify-only and Abort preserve boot selection | CLI tests and board version/slot checks |
| Explicit verified-session Commit and reboot | Tests and hardware activation in both slot directions |
| No replay, lost commit ACK and metadata errors | Tests include actual TCP cuts before/after Commit and storage failure |
| Cancellation, disconnect and exclusive ownership | Tonic tests and repeated board Begin/Abort/resubscription probes |
| Five-second upload inactivity | Virtual-clock tests, live-log TCP test and hardware PING/idle probe |
| Retained status, boot inventory and logs | Existing read-only TCP endpoint plus gRPC status/streaming logs |
| Concurrent transfer/log progress | One-connection Tonic and hardware probes, including a stalled log window |

TLS/authentication, automatic rollback and dependable power-loss recovery were
not implemented by legacy OTA and are not claimed as parity guarantees.

## Automated validation

`cargo test` includes **64 tests**. All pass. Transport tests accept exactly one
TCP connection and use independent Tonic/h2 clients against the actual Handler.
Coverage includes malformed protobuf/ACKs, all metadata/digest/order categories,
pre-verification commit rejection, activation write blocking, before/after-commit
TCP loss, log errors without loss of commit context, cancellation, deadlines,
flow-control isolation, overwritten history and read-only rejection of every
old upload opcode. The watchdog's reset/clear/expiry behavior is tested with
virtual time; another TCP test generates live logs during an abandoned upload.

The ESP32-S3 default release build passes without extra features. Formatting and
patch whitespace checks pass. Clippy is unavailable on the installed newer
nightly; the default Clippy selects Rust 1.79. Host mocks do not establish flash
power-loss behavior or worst-case scheduling/heap/stack bounds.

## Hardware evidence, 2026-11-04

Identity checked before writes: `esp32s3-a4:cb:8f:d8:13:c8` at `10.128.248.68`.
Expected layout: 16 MiB flash with two 3 MiB application slots.

- Initial grpc-streams-2: 636592 bytes verified, 156 uniquely identified live
  chunk logs before the next chunk, one connection, 29.5871 seconds.
- Stalled logs on that build: the same image verified with consumption paused
  behind a 1024-byte stream window; overwritten history reported on resumption.
- gRPC activation with live logs: grpc-commit-3 into ota_0, grpc-ota-1 into ota_1,
  then grpc-ota-2 into ota_0. Verify-only preserved the old version/slot.
- Post-removal grpc-only-1: 634000 bytes uploaded/committed over gRPC, then
  confirmed running in ota_1. The hardware parity probe observed idle expiry in
  4.9815 seconds despite PINGs and an open Logs stream, proved lease release with
  a fresh Begin/Abort, and rejected all five old OTA operations on port 7779.
- That gRPC-only firmware then verified a 634000-byte image with log consumption
  stalled (29.5609 seconds, 155 chunks), preserving its running version/slot.
  A subsequent upload with concurrent live logs committed grpc-only-2 into ota_0.
  This update was performed by firmware with no legacy OTA writer present.
  Post-reboot grpc-only-2 passed the parity probe again (4.9982-second expiry),
  rejected all legacy write operations, and advertised the expected endpoints.

The final review replaced string-matched lifecycle control with typed events.
That source built and passed all 64 tests, uploaded/committed a 634064-byte image
with live logs, and booted `grpc-review-1` in ota_1. The hardware idle probe
expired in 4.9998 seconds despite PINGs; legacy-write rejection, lease release,
four cancellation/resubscription cycles and empty-upload rejection passed.

Earlier allocator estimates peaked at 71872 bytes of the configured 102400-byte
heap. Those are boot-wide estimates from the previous build, not a current
worst-case memory guarantee. Synchronous flash work can delay the executor.

Hardware probes and Tonic integration tests live under `tools/espctl` with
shared host-only support in `tests/support/grpc.rs`. The portable `device-grpc`
library keeps its watchdog tests and has no hardware examples or Tonic dependency.

## Reproduce

Inside `nix develop`, with Wi-Fi build settings exported privately:

```sh
cargo test
cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
# Export an application-only image using the provisioning guide; keep it private.
cargo run -p espctl -- upload --host BOARD_IP --image IMAGE.bin --firmware-version VERSION --logs --commit
cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-cleanup -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-parity -- BOARD_IP IMAGE.bin VERSION
```

`grpc-concurrent`, `grpc-cleanup` and `grpc-parity` are non-activating probes. The live concurrent
probe requires uniquely identified chunk logs before sending the next chunk;
stale retained records cannot count as current progress. The stalled variant
requires a dropped-history indication on resumption. See [provisioning and
recovery](../ota/provisioning.md) and [test limits](../ota/testing.md).
