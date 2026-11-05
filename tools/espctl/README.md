# espctl: device status, profiling, and development OTA

A Mac CLI for querying and updating one board by IP using
bounded Protobuf messages. One Management gRPC/HTTP2 service on TCP 7779 serves
status, profiling, log snapshots/streams and uploads. Upload and live logs share
one connection; a second client can query the same port concurrently.
Updates require provisioned `device-demo` firmware. Legacy OTA and `--legacy`
have been removed; recovery uses USB/BOOT/RESET.

## Discover boards

```sh
nix develop
cargo run -p espctl -- discover
cargo run -p espctl -- discover --watch
```

Lists resolved DNS-SD services on the same IPv4 LAN, with their advertised port,
firmware, management state, and OTA capability. The Mac uses `mdns-sd`; firmware
uses `edge-mdns`. TXT metadata may be cached; use status for live uptime. An empty
scan does not prove a board is offline. See [interface selection and DNS-SD records](../../docs/device-management/discovery.md).

## Build and query

From the repository root:

```sh
nix develop
cargo run -p espctl -- --help
cargo run -p espctl -- status --host 192.168.1.123
```

Ordinary Cargo commands build for the Mac from the repository root using the
workspace lockfile. For repeated queries, run `target/debug/espctl` directly
after the first build.

Clap provides argument validation and generated help:

```sh
cargo run -p espctl -- --help
cargo run -p espctl -- status --help
```

Options: `--port 7779`, `--timeout-ms 5000`. The host is an IP literal, not a URL.
Success prints identity, application, version, uptime, OTA availability,
and total query latency in milliseconds. Timing includes TCP connection setup,
refused-connect retries, request/response I/O, and response validation. It excludes
Cargo startup/build time, argument parsing, and output rendering; this is not a
pure network RTT measurement. Failed queries also report elapsed milliseconds.
Failure prints an error to stderr and exits nonzero. Refused connection attempts
are retried within the total deadline; an established request is never replayed.
The device is not authenticated.

## Development OTA uploads

See [build, export, provisioning, and upload guide](../../docs/ota/provisioning.md).
An upload verifies but does not activate unless `--commit` is supplied:

```sh
cargo run -p espctl -- upload --host 10.128.248.68 \
  --image target/ota/ota-next.bin --firmware-version ota-next --logs --commit
```

Upload defaults: `--port 7779`, `--timeout-ms 5000`, 180-second total deadline.
The firmware also releases an upload after five seconds without an accepted
upload operation, even when HTTP/2/log traffic continues.

The release string must match the embedded version. Images must be exported
application binaries built from `device-demo`; sensor images, ELF files,
and merged images are rejected. After commit, reconnect and
check version and running slot. Commit-response loss is an uncertain outcome.

## Hardware regression probes

Optional host-side probes exercise the known development board. They check its
identity before upload admission and do not activate an image:

```sh
cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-cleanup -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-parity -- BOARD_IP IMAGE.bin VERSION
```

These check per-chunk live logs, flow-control isolation, cancellation, idle
expiry, concurrent status clients and closure of the retired port 7780. Tonic integration tests
and shared host helpers also live in this package. See [test details and
limits](../../docs/ota/testing.md).

## Inspect flash/boot layout

```sh
cargo run -p espctl -- status --host 10.128.248.68 --boot
```

Requires `device-demo` firmware with boot inventory support. The query is
read-only; OTA availability depends on the supported partition layout. The reported configured flash capacity comes from the image
header,
not a physical chip probe. See [partition foundations](../../docs/ota/provisioning.md)
for the build/flash command, candidate layout, and offline image export.

## Profile query latency

```sh
cargo run -p espctl -- status --host 10.128.248.68 --timings
cargo run -p espctl -- profile --host 10.128.248.68 --samples 50 --interval-ms 100
cargo run -p espctl -- profile --host 10.128.248.68 --samples 50 --interval-ms 100 --csv > profile.csv
```

`profile` reports min, median, nearest-rank p95, max, and mean across successful
queries. Each sample opens a fresh connection. Failures go to stderr and cause a
nonzero exit after sampling; CSV has one row per successful query. The interval
is a pause between completed samples, excluded from latency. `--timings` splits
status into TCP connect (including retry wait), protobuf encode, RPC response
(including HTTP/2 setup, send, server processing, body and trailers), and local
decode/validation. Retry wait is already included in connect.
Defaults: 30 samples, 100 ms interval. Max samples: 1000.

Optional device diagnostics report decode and response preparation microseconds.
Request read time is unavailable at the gRPC handler boundary, so its reserved
field is zero. Device durations include task preemption and overlap host phases.
CSV columns are `sample,total_ms,connect_ms,retry_wait_ms,encode_ms,rpc_ms,decode_validate_ms,refused_connects,device_decode_us,device_prepare_us`.

## Provision the first board

Use the dedicated `device-demo` firmware for device-management work. It starts
Wi-Fi, DHCP/reconnection, and the gRPC Management service. It requires no sensors,
LED configuration, UDP destination, or OTA partition changes. Connected IMU wiring
can stay in place; this firmware does not access it.

Export `WIFI_SSID` and `WIFI_PASSWORD` in your regular terminal before entering
`nix develop` (or put the exports in your interactive shell configuration). An
optional `FIRMWARE_VERSION` labels the build; otherwise it uses the package version.
Nix supplies tools, and the child development shell inherits exported variables.
No Python wrapper or automatic `.env` loader is used. The existing local `.env`
can stay on disk, git-ignored, but plain Cargo does not read it.

From the repository root in the development shell:

```sh
cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
espflash flash --monitor --port /dev/cu.usbmodem5C4C1816511 \
  target/xtensa-esp32s3-none-elf/release/device-demo
```

Use your actual serial port if different. This replaces the sensor application
with the device-management demo. Wi-Fi credentials are embedded in the firmware.
Read the DHCP IPv4 address and TCP listener message from serial output. In another
development-shell terminal, run:

```sh
cargo run -p espctl -- status --host <board-ip>
```

Hardware acceptance checklist:

- Confirm real device ID, `application: device-demo`, version, and OTA unavailable.
- Query repeatedly; uptime should increase and reconnects should work.
- Interrupt a partial request, then query after the five-second session deadline.
- Disconnect/reconnect Wi-Fi and check status recovers after DHCP reconnects.
- Test two boards separately and confirm distinct device identities.

The IMU and soil demos use `wifi-network` plus `wifi-telemetry` for their existing
sensor/UDP behavior; they no longer have a `status` feature. Integrating the proven
management service into sensor firmware is a later step. The shared service takes
an existing network stack, so it will not require another radio or duplicate DHCP.

## Iterate without hardware

The host-only mock uses the same Protobuf contract and handler. It does not emulate
Wi-Fi, the Embassy listener, flash, or sensors. In a development shell, from the
repository root, build and run it normally:

```sh
cargo run \
  -p espctl --example mock-device -- 127.0.0.1:7779
```

In a second terminal:

```sh
cargo run -p espctl -- status --host 127.0.0.1
```

Stop the mock with Ctrl+C. The response is explicitly identified as `mock-esp32s3`.

Run host protocol/client tests from the repository root in the development shell:

```sh
cargo test \
  -p ota-core -p ota-protocol -p espctl
```

Cross-compile firmware without a board using nonsecret build-only placeholders:

```sh
WIFI_SSID=build-check WIFI_PASSWORD=build-check \
  cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
```

Rebuild with real network settings before flashing. Placeholder builds cannot
connect to your Wi-Fi. See [docs/ota/protocol.md](../../docs/ota/protocol.md) for the contract
and [docs/ota/design.md](../../docs/ota/design.md) for the remaining OTA milestones.

See [OTA automated tests and manual fault harness](../../docs/ota/testing.md) for the validation
matrix, board probe modes, physical fault procedures, and result template.

## Recent management logs

```sh
cargo run -p espctl -- logs --host <device-ip>
cargo run -p espctl -- logs --host <device-ip> --follow
```

Requires log-enabled firmware. A snapshot pages through the retained entries; `--follow`
polls every 500 ms when caught up and retries connection failures. The firmware
keeps 32 entries in RAM, each with a boot-scoped sequence, uptime in milliseconds,
severity, and up to 64 UTF-8 bytes of message text. Four entries fit in one existing
512-byte control frame. Boot IDs are random correlation tokens, not authentication.
A changed boot ID or invalid cursor resets pagination; overwritten entries produce
a warning. Output escapes terminal control characters.

This first version records management startup, image-confirmation failure, upload
state transitions, and failed/timed-out sessions. It does not capture all serial
output or pre-service Wi-Fi startup. Reading logs does not itself add an entry.
Status and log polling remain available on the same port during an upload through a second connection.
For server-push logs on the upload connection, use `upload --logs`. Slow readers
may miss overwritten entries. Old firmware may return an unsupported-operation error. The TUI can reuse `espctl::read_logs` and
its cursor without a separate backend.
