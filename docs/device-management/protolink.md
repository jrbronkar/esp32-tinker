# Concurrent gRPC management and OTA

`device-demo` exposes one cleartext HTTP/2 service,
`esp.experimental.Management`, on TCP **7779**. It serves status, log snapshots,
log streams and firmware uploads. Two bounded client connections share one
exclusive flash lease. DNS-SD advertises the management endpoint over UDP 5353;
see [discovery](discovery.md).

Protolink is pinned to `62cc2c87a687ba65fc6885e7c9691cefacf2d064`.
`Cargo.lock` pins zerodds HTTP/2 and HPACK to 1.0.0-rc.7. The handler uses
fixed-capacity micropb messages. Schema compatibility uses additive Protobuf
changes with preserved field numbers and meanings; breaking API or behavior
changes require a new service/package. Upload envelope field 1 is reserved.

## API and resource limits

The [service schema](../../protocol/management.proto) defines four RPCs:

| RPC | Shape | Request → response |
| --- | --- | --- |
| Status | Unary | `StatusRequest` → `StatusResponse` |
| ReadLogs | Unary | `ReadLogsRequest` → `ReadLogsResponse` |
| Logs | Server streaming | `ReadLogsRequest` → stream of `ReadLogsResponse` |
| Upload | Bidirectional streaming | stream of `UploadRequest` → stream of `UploadResponse` |

Status and log messages are direct RPC payloads. Only Upload carries an operation
oneof and request IDs, to correlate ordered operations with acknowledgments on
its persistent bidirectional stream. RPC failures use gRPC status trailers.

See the [protocol contract](../ota/protocol.md) for message and lifecycle details.
The configured bounds are:

| Resource | Limit |
| --- | --- |
| Client connections | 2 |
| Active RPC streams per connection | 2 |
| Log subscription slots per connection | 2 |
| Flash leases across all connections | 1 |
| Pending acknowledgments per upload | 1 |
| Receive window per stream | 8192 bytes |
| Connection receive window | 65535 bytes, the HTTP/2 minimum |
| Decoded header list | 1024 bytes |
| Pending serialized output per connection | 8192 bytes |
| Upload request message | 4608 bytes |
| Status/log request message | 512 bytes |
| Response message | 512 bytes |

Compression is disabled. The Embassy driver polls TCP read and write halves
independently. Upload and Logs can progress on one connection, while another
client queries the same endpoint. A competing upload fails admission before
writing flash.

## Upload lifecycle and deadlines

Begin validates application, chip, firmware version, image size and SHA-256,
then acquires the flash lease. Ordered sector-sized chunks write directly to the
inactive slot; the device never buffers a complete image. Clients await each
acknowledgment and validate request ID, phase, next offset and target slot.
Finish reads flash back and validates its digest and ESP image structure.

Verification alone preserves boot selection. Commit explicitly activates the
verified image and reboots. Activation is latched before boot metadata writes,
and further writes remain blocked until reset, including after a metadata error.
The driver allows two seconds for the acknowledgment/trailers, attempts a bounded
TCP flush and resets. Disconnect during pending activation also triggers reset.
A valid Committed acknowledgment establishes success even if trailers are
truncated. A lost or invalid acknowledgment leaves activation uncertain; inspect
the running firmware and slot before retrying. Operations are never replayed
automatically.

Each connection has its own five-second upload inactivity watchdog. Only accepted
upload operations refresh it; log traffic and HTTP/2 keepalives do not. Expiry
closes that connection, cancels its RPCs and releases flash. Abort, cancellation
and completion clear the upload deadline. TCP inactivity is bounded to ten
seconds and connection lifetime to 180 seconds. RPC deadlines can expire sooner.

## Retained logs

`device-grpc::logs::LogStore` owns retained storage and uses Embassy
`MultiWakerRegistration` for bounded reader notifications. Firmware appends and
reads logs through this abstraction. Snapshot polling preserves streaming
registrations. Typed lifecycle events drive state, watchdog and reboot behavior;
log text is presentation.

Logs use a 32-record RAM ring, four-entry batches, boot-scoped cursors and
an overwritten-history indication. A stalled log reader cannot consume the
upload stream's receive window. When reading resumes, the cursor reports any
history overwritten in the bounded ring.

## Validation and limits

The host suite has **68 passing tests**, including independent Tonic/h2 peers
against the actual handler. Coverage includes metadata, firmware/image/digest
and ordering checks; malformed messages and acknowledgments; unknown Protobuf
fields; status options; log polling and notification; competing clients;
cancellation and deadlines; live and stalled log streams; and actual TCP loss
before and after Commit. Tests also cover activation write blocking, uncertain
commit outcomes and no replay. Upload watchdog tests use virtual time and a
TCP peer producing logs during an abandoned upload.

The ESP32-S3 release build and all `espctl` targets pass. Current hardware
validation on one 16 MiB ESP32-S3 with two 3 MiB application slots confirms:

- A 625088-byte image uploaded with concurrent logs, committed and booted in
  ota_0; status reports idle and OTA available.
- A full verify-only upload received all 153 matching chunk logs before sending
  their next chunks.
- A full upload verified with log consumption stalled behind a 1024-byte window;
  resuming consumption reported overwritten history.
- Four cancellation/resubscription cycles released upload ownership, and a fresh
  Begin/Abort succeeded. Empty uploads were rejected.
- Snapshot polling and a second status client worked during streaming.

These runs do not establish worst-case heap/stack usage, scheduling latency,
long-duration reliability or power-loss recovery. Synchronous flash work can
pause the executor. TLS/authentication, signatures and automatic rollback remain
unimplemented or unverified; this is unsigned development OTA. USB/BOOT/RESET
is the recovery path.

## Reproduce

Host integration tests and hardware probes live in `tools/espctl`; the portable
`device-grpc` library contains log-store and watchdog tests. Inside `nix develop`,
with Wi-Fi build settings exported privately:

```sh
cargo test
cargo check -p espctl --all-targets
cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
# Export an application-only image using the provisioning guide; keep it private.
cargo run -p espctl -- upload --host BOARD_IP --image IMAGE.bin --firmware-version VERSION --logs --commit
cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-cleanup -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-parity -- BOARD_IP IMAGE.bin VERSION
```

The three probes do not activate images. The live concurrent probe requires
uniquely identified chunk logs before sending the next chunk; retained records
cannot count as current progress. The stalled variant requires an
overwritten-history indication on resumption. See [provisioning and
recovery](../ota/provisioning.md), [schema compatibility](../ota/protocol.md#schema-compatibility)
and [test limits](../ota/testing.md).
