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
exclusive flash ownership, idle expiry despite PINGs and closure of port 7780.

## Validation status

One 16 MiB ESP32-S3 with two 3 MiB slots has passed full-image verification,
self-update with concurrent logs and explicit commit, post-reboot slot checks,
stalled logs, concurrent polling/profiling, cancellation and idle cleanup.
Host mocks and these runs do not establish production readiness: authentication,
signed releases, rollback/health deadlines, power-cut recovery, worst-case
memory/latency and long-duration reliability remain open. See [production gaps](design.md#production-gaps).
Keep binaries, credentials and detailed run logs outside the repository.
