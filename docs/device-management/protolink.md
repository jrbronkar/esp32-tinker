# Experimental concurrent Protolink transport

The opt-in `experimental-grpc` feature on `device-demo` serves cleartext HTTP/2
on TCP 7780, alongside the legacy management/OTA listener on 7779. Discovery
continues to advertise the legacy endpoint. Protolink is pinned to
`62cc2c87a687ba65fc6885e7c9691cefacf2d064`; Cargo.lock pins zerodds HTTP/2 and
HPACK to 1.0.0-rc.7. The raw Handler keeps existing micropb 0.7 protobuf bodies
independent of Protolink's optional micropb 0.6 codec.

## RPC contract

See `protocol/management.proto`. Every request/response uses the existing
`ota.Request`/`ota.Response` envelope, without its legacy four-byte length
prefix. Protolink supplies HTTP/2 and gRPC framing/status trailers.

- `Status`: unary, with current uptime and management state.
- `Logs`: server streaming, one ReadLogs cursor request and bounded batches of
  four records. A first snapshot is always sent; later batches arrive when
  records are available, including reset/overwritten-history indications.
- `Upload`: bidirectional streaming. Send Begin, ordered sector-sized chunks,
  Finish, then Abort or half-close. Wait for the matching ACK before sending
  each operation. Finish hashes and validates the image read back from flash.
  Abort/half-close releases the lease without changing boot selection. Commit
  is rejected; activation still requires the legacy explicit OTA path.

One task owns the flash target behind a RefCell. Either transport borrows it
exclusively for the upload's lifetime; competing uploads fail before writing.
Cancellation, malformed operations, half-close and connection/driver teardown
release the lease. An upload's synchronous flash operation can briefly delay
other work, while its network waits permit logs and legacy status to progress.
The complete image is never held in firmware RAM.

There are at most two active streams, two fixed subscription slots, one upload
state and one pending upload ACK. Receive windows are 8192 bytes per stream so
a complete 4096-byte protobuf chunk can arrive without awaiting its own
consumption. The connection window is 65535 (the HTTP/2 minimum), decoded
headers are capped at 1024, serialized output at 8192 and messages at 4608.
Compression is disabled. This is not a proof of a global heap bound.

The Embassy server splits the TCP socket into independently polled directions.
External log producers register/wake the connection task through Runtime.
Connections have a ten-second socket inactivity timeout and a 180-second
absolute lifetime. Clients must reconnect after expiry or network loss.

## Automated tests

`cargo test` includes `device-grpc` by default. The existing 48 tests plus five
Tonic transport tests pass (53 total). The new tests use independent Tonic/h2
clients against the actual Handler over TCP; each fixture accepts exactly ONE
connection, so requests cannot silently move to separate sockets.

Coverage:

- Repeated status calls, malformed protobuf and unknown-method trailers.
- A valid 128 KiB synthetic ESP image uploaded and verified, requiring each
  matching chunk log BEFORE the next chunk; upload-then-log serialization fails.
- A 1024-byte log receive window with consumption stopped while three complete
  uploads finish; resumption reports overwritten RAM history.
- Repeated cancellation/resubscription, external log wakeups, competing legacy
  flash ownership, invalid chunk order and teardown during an active upload.
- Competing RPC upload streams cannot release each other's lease; log deadlines
  produce DEADLINE_EXCEEDED trailers.

Both legacy and experimental release firmware builds pass. Formatting and
patch whitespace checks pass. Clippy was unavailable: the default installation
selects Rust 1.79, and the installed newer nightly has no Clippy component.

Host mocks exercise the real updater/image validator but do not establish flash
hardware behavior, power-loss recovery or worst-case scheduling latency.

## Hardware validation, 2026-11-04

Identity checked before writes: `esp32s3-a4:cb:8f:d8:13:c8`, address
`10.128.248.68`. Existing OTA activated `grpc-streams-1` in ota_0; boot status
confirmed the version, expected 16 MiB layout and OTA availability.

| Probe on grpc-streams-1 | Observed result |
| --- | --- |
| Live logs + real upload, one HTTP/2 connection | 636448 bytes verified from inactive-slot flash; all 156 chunk-specific live logs arrived before the next chunk; 32.4395 s, max chunk+log step 474.7 ms |
| Logs deliberately unread, 1024-byte receive window | Same full image verified in 34.7162 s; max chunk step 431.9 ms; overwritten history reported when reads resumed |
| Legacy status during gRPC upload | Responded in 79.24 ms and reported receiving |
| Four upload cancellations + log resubscriptions | Subsequent Begin/Abort succeeded; empty upload rejected; same connection |

The gRPC probes were verify-only: they did not activate the uploaded image.
The second probe logged allocator estimates: 47528 bytes used after connection
cleanup, 70268-byte boot-wide peak, 54872 currently free of the 102400-byte heap.
These are allocator estimates, not stack measurements or a worst-case bound.

A final review fixed target-offset reporting when a connection opens while the
legacy service owns flash. `grpc-streams-2` contains that correction; final
activation/probe results are recorded below.

Final `grpc-streams-2` activation was verified in ota_1. The live-stream probe
verified 636592 bytes and all 156 matching chunk logs in 29.5871 seconds
(maximum chunk step 468.7 ms), including checking the ACK target against the
inactive partition from boot inventory. The final stalled-log probe also verified
636592 bytes, in 29.2389 seconds (maximum chunk step 464.4 ms), and reported
lost history on resumption. The final cancellation/resubscription and empty
upload probe passed too. Both completed uploads left boot selection unchanged.
Final post-probe status confirmed `grpc-streams-2`, ota_1, idle and OTA available.
Final allocator estimates were 47920 bytes used, a 71872-byte boot-wide peak
and 54480 currently free. The observed peak leaves 30528 bytes of the configured
heap; this is still not a worst-case memory guarantee.

## Reproduce

Enter `nix develop` at the repository root. Export Wi-Fi credentials locally
without printing them, then build/export using the existing provisioning guide:

```sh
cargo test
FIRMWARE_VERSION=grpc-streams-2 cargo build --target xtensa-esp32s3-none-elf \
  -Zbuild-std=core,alloc --release -p device-demo --features experimental-grpc
# Export an application-only image with espflash; keep credential-bearing images private.
```

After identity/layout checks and explicit activation through legacy OTA:

```sh
cargo run -p device-grpc --example concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p device-grpc --example concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p device-grpc --example cleanup -- BOARD_IP IMAGE.bin VERSION
```

The examples check the known board identity before beginning an upload. The
concurrent probe checks unique request-ID/offset log records to avoid counting
retained records from an earlier upload as current progress. The stalled mode
requires a dropped-history indication after resumption. Both use one Tonic
channel while both RPC streams are active on the single-listener connection.

## Remaining limits

TLS/authentication, full serial-log capture, gRPC activation, power interruption
and Wi-Fi outage tests, worst-case heap/stack analysis and sensor-task latency
instrumentation remain outstanding. Flash verification is synchronous and can
pause the executor. The endpoint is an unauthenticated development experiment;
automatic rollback and dependable power-loss recovery remain unverified.
