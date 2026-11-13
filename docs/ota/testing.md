# OTA validation

Run inside `nix develop`; host tests require no board or credentials:

```sh
cargo test
cargo check -p espctl --all-targets
```

Host tests cover typed/empty Protobuf requests, bounded decoding, independent
Tonic/h2 interoperability, image/metadata/digest/order validation, explicit
Commit, uncertain outcomes and no replay, TCP loss, competing clients,
cancellation, deadlines, independent watchdogs, log wakeups and flow control.
The ESP32-S3 release build also passes.

## Hardware probes

Use a private application-only image built as described in [provisioning](provisioning.md).
The four probes run sequentially against the known development board and
verify the candidate in the inactive slot without activating it:

```sh
nix develop --command python3 tools/ota-probes/run.py BOARD_IP IMAGE.bin VERSION
```

The runner reports each result and exits nonzero if any probe fails. It is
intended for a dedicated board with network access. GitHub CI automatically
runs the host regression suite through `cargo test --locked`, including the
embedded gRPC deadline and allocation-failure tests.

The live probe requires each chunk log before the next chunk. Stalled mode holds
a 1024-byte log window while upload completes, then requires overwrite reporting.
Cleanup tests cancellation/resubscription. Parity tests concurrent status,
exclusive flash ownership, idle expiry despite PINGs, client deadline expiry
with flash lease reacquisition, and closure of port 7780.

## Heap pressure on the development board

Connect the board's USB serial port before activating the instrumented image.
Build a private IMU image with allocator statistics and a 4 KiB test reservation:

```sh
OTA_HEAP_RESERVE_BYTES=4096 FIRMWARE_VERSION=embedded-grpc-heap-probe \
  cargo build --locked --target xtensa-esp32s3-none-elf \
  -Zbuild-std=core,alloc --release -p device-demo --features imu,ota-heap-probe
espflash save-image --chip esp32s3 --flash-size 16mb \
  target/xtensa-esp32s3-none-elf/release/device-demo /private/tmp/imu-heap-probe.bin
```

Use the normal OTA activation procedure and verify the version and boot slot.
With the USB monitor recording, run the OTA probe suite against the booted
image. The parity probe overlaps Logs, an active Upload, and a second client's
Status. Record the reservation, peak heap use, minimum aggregate free bytes,
image size, and upload times. The allocator high-water mark is cumulative
since boot and aggregate free bytes do not measure the largest free block.
Reinstall the normal firmware after the probe.

## Validation status

One 16 MiB ESP32-S3 with two 3 MiB slots has passed full-image verification,
self-update with concurrent logs and explicit commit, post-reboot slot checks,
stalled logs, concurrent polling/profiling, cancellation and idle cleanup.
Host mocks and these runs do not establish production readiness: authentication,
signed releases, rollback/health deadlines, power-cut recovery, worst-case
memory/latency and long-duration reliability remain open. See [production gaps](design.md#production-gaps).
Keep binaries, credentials and detailed run logs outside the repository.
