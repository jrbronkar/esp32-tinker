# OTA validation and feature parity

Run from the repository root inside `nix develop`. Host tests need no board,
Wi-Fi credentials or firmware image:

```sh
cargo test
cargo check -p espctl --all-targets
```

The suite contains 64 tests. The gRPC migration reuses ota-core validation;
transport tests exercise the actual Handler against independent Tonic/h2 peers.
Tests cover metadata/image/digest/order rejection, verify-only, explicit commit,
lost ACKs and actual TCP loss before/after Commit, no replay, activation write
blocking, cancellation, deadlines, competing streams and overwritten logs.
The portable watchdog has virtual-time tests, plus a TCP test that produces
live logs while the upload is abandoned. Port 7779 rejects every upload opcode.

## Hardware probes

Use a provisioned board with a compatible private application-only image, and
record identity/version/running slot before writes. See [provisioning](provisioning.md).

```sh
cargo run -p espctl -- status --host BOARD_IP --boot
cargo run -p espctl -- upload --host BOARD_IP --image IMAGE.bin --firmware-version VERSION --logs
cargo run -p espctl -- upload --host BOARD_IP --image IMAGE.bin --firmware-version VERSION --logs --commit
cargo run -p device-grpc --example concurrent -- BOARD_IP IMAGE.bin VERSION
GRPC_STALL_LOGS=1 cargo run -p device-grpc --example concurrent -- BOARD_IP IMAGE.bin VERSION
cargo run -p device-grpc --example cleanup -- BOARD_IP IMAGE.bin VERSION
cargo run -p device-grpc --example parity -- BOARD_IP IMAGE.bin VERSION
```

`concurrent` requires every uniquely identified chunk log before sending the
next chunk, then verifies flash without activation. Its stalled mode stops log
consumption behind a 1024-byte stream window; upload must still finish and log
resumption must report overwritten history. `cleanup` tests cancellation and
resubscription. `parity` abandons an upload with HTTP/2 keepalives and an open
log stream, requires expiry around five seconds, checks a new Begin/Abort, and
requires port 7779 to reject Begin/Chunk/Finish/Commit/Abort.
The examples check the known development board identity before upload admission.

Legacy OTA, its CLI flag and the old upload-fault-probe harness are removed.
Current [parity and hardware results](../device-management/protolink.md) distinguish
new gRPC evidence from earlier legacy results below.

## Still-unverified physical cases

Power interruption during sector/metadata writes, AP outages, rollback-enabled
bootloader behavior, bounded health confirmation, sensor latency, worst-case
heap/stack and long-duration stress need separate instrumented runs. A paused
software test or a host memory mock does not establish torn-write recovery.
Keep private binaries/backups and exact board/bootloader configuration with run
records. Do not treat a missing Commit ACK as proof that activation failed.
TLS/authentication and signed releases remain unimplemented for both the old and
new transport; feature parity is not a security assurance.

## Historical legacy-transport results

| Build / check | Evidence | Limits |
| --- | --- | --- |
| Prior legacy host suite | 40 tests pass: 10 discovery/client, 14 status/client, 3 upload/client, 5 core, 8 protocol | Mock I/O/storage cannot establish real flash or bootloader recovery |
| Prior legacy firmware | Firmware check and release link pass | Startup error reset path has not been fault-injected |
| `ota-merge-check-1` on board | Earlier no-feature release: 501,024 bytes uploaded, verified and committed over Wi-Fi from ota_0; post-reboot status confirms protocol 1, ota_1, OTA available | Earlier activation verified; rollback and confirmation-error injection remain untested |
| `discovery-final-1` on board | Wi-Fi activation, discovery, status and boot inventory pass. Unrelated/type/class queries are ignored; receiving/verified DNS snapshots and disconnect cleanup to idle pass without reboot | One board; DHCP changes, hostname conflict handling and long-duration network behavior remain unverified |
| Discovery network comparison | Alternate AP: 10/10 scans passed; status and boot inventory passed | Multicast remains intermittent on another WLAN; exact cause unresolved |
| `unified-1` on board | Eleven non-activating probes and 500,560-byte verify-only upload pass | One socket timeout before a successful fresh transfer; activation/rollback not tested on this build |
| Earlier A → B → C prototype | Wi-Fi activation alternated slots; earlier NVS comparison was identical | Predates unified protocol; not evidence for every later build |

Keep detailed run logs, firmware images, and raw backups outside the repository.
Use the template above for local records and update this summary when validation
changes; distinguish new evidence from results on earlier builds.
