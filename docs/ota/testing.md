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
These probes check the known development board identity and do not activate images:

```sh
cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p espctl --example grpc-concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-cleanup -- BOARD_IP IMAGE.bin VERSION
cargo run -p espctl --example grpc-parity -- BOARD_IP IMAGE.bin VERSION
```

The live probe requires each chunk log before the next chunk. Stalled mode holds
a 1024-byte log window while upload completes, then requires overwrite reporting.
Cleanup tests cancellation/resubscription. Parity tests concurrent status,
exclusive flash ownership, idle expiry despite PINGs and closure of port 7780.

## Validation status

One 16 MiB ESP32-S3 with two 3 MiB slots has passed full-image verification,
self-update with concurrent logs and explicit commit, post-reboot slot checks,
stalled logs, concurrent polling/profiling, cancellation and idle cleanup.
Host mocks and these runs do not establish production readiness: authentication,
signed releases, rollback/health deadlines, power-cut recovery, worst-case
memory/latency and long-duration reliability remain open. See [production gaps](design.md#production-gaps).
Keep binaries, credentials and detailed run logs outside the repository.
