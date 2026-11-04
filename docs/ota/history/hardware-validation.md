# Hardware validation history

These are dated observations from different builds, not claims that the latest
firmware passed every earlier test. See [current validation status](../testing.md#validation-status).

## Current evidence (2026-11-03)

- Expanded host suite: **29 tests pass** (14 status/client, 3 upload/client,
  5 update-core, 7 protocol). The acknowledgement rejection test exercises eight
  different failure variants; storage injection exercises read, write, and commit.
- Manual harness and other CLI examples compile for the Mac target.
- Earlier board evidence: A → B → C activation, verify-only, disconnect,
  digest rejection, and NVS comparison; see docs/ota/design.md.
- Eleven non-activating board probe modes now pass on the connected ESP32-S3;
  see the hardware run below. Paused physical-fault and valid-commit modes remain
  unexecuted in this run.


### Hardware probe run (2026-11-03)

Board: ESP32-S3, device ID `esp32s3-a4:cb:8f:d8:13:c8`, TCP port 7779.
Baseline: `ota-c`, running `ota_0` at `0x10000`; OTA available. Test candidate
was exported from the existing `ota-c` dev-ota ELF, 503,392 bytes, SHA-256
`abdda9409ec74373a15d38690de2e39816d3910016d37ebfaa47e8eb4416c1d6`.
Harness source: staged changes on base revision `1d3c523` (not yet committed).
Candidate binary and raw logs remain local and are not committed.

| Probe | Result | Elapsed seconds |
| --- | --- | ---: |
| `malformed` | PASS | 0.55 |
| `oversized` | PASS | 0.36 |
| `truncated` | PASS | 0.45 |
| `bad-protobuf` | PASS | 0.45 |
| `stall` | PASS | 7.38 |
| `bad-offset` | PASS | 1.31 |
| `early-commit` | PASS | 1.34 |
| `early-finish` | PASS | 1.1 |
| `bad-id` | PASS | 0.89 |
| `disconnect` | PASS | 1.15 |
| `wrong-digest` | PASS | 18.72 |

Every case checked that the original version remained and uptime increased;
malformed-session cases additionally required connection closure and listener
recovery. The wrong-digest case transferred the full image and observed the
explicit stored-image digest rejection. No valid Commit was sent in these cases.
This run did not capture serial traces or reread NVS/PHY; earlier NVS evidence
must not be treated as a fresh comparison for this campaign.

A full verify-only upload also passed: 503,392 bytes verified in inactive slot
`0x310000`, followed by Abort (no activation). Final `status --boot` confirmed
`ota-c` still running from `ota_0` at `0x10000`, uptime 11,240,661 ms and OTA
available. Baseline uptime was 11,122,223 ms; no reboot was observed in this run.


### Unified protocol regression (after the hardware campaign)

Status and upload now share version 1. All **30 host tests** pass, including a
new shared-version/disabled-upload-handler regression; upload mock peers assert
version 1 on every request. Both firmware configurations and the hardware probe
compile. The capability marker changed to `rust-dev-ota-v1`; the earlier board
requires USB reprovisioning and a repeat of the hardware suite. No new hardware
result is claimed for this protocol change.


### Unified firmware USB migration (2026-11-03)

Built release `unified-1` with local Wi-Fi settings using Nix, target ESP32-S3,
`dev-ota`, and explicit core/alloc build flags. Image: 500,560 bytes; SHA-256
`27028f4eb6c0fcbaebf7a0645bfdb54be74761771c689b719892de38438a97ee`;
capability marker `rust-dev-ota-v1`. Flashed the connected 16 MiB board over USB
using the existing partition CSV, ota_0, and OTA metadata initialization. NVS/PHY
were not erased; no fresh byte-for-byte preservation comparison was performed.
Serial showed bootloader load offset `0x10000`, successful DHCP at 10.128.248.68,
and development OTA available. A manual reset was used to capture startup logs.

Initial network queries timed out/reported host unreachable while reachability
was settling, then status succeeded. The first harness attempt also encountered
a refused initial connection while the sole listener rearmed. The probe now
retries only connection establishment refusals within five seconds; transmitted
operations are never replayed. After rebuilding it, all eleven non-activating
modes passed: malformed, oversized, truncated, bad-protobuf, stall, bad-offset,
early-commit, early-finish, bad-id, disconnect and wrong-digest. Each successful
probe verified the original version and increasing uptime. Raw results are kept
locally; firmware images containing credentials are not committed.

This exercises unified upload dispatch and failure recovery, not successful
activation or rollback. The newly added startup initialization-error reset path
was not deliberately fault-injected.

The first normal verify-only upload returned `Resource temporarily unavailable`
from socket I/O; status immediately afterward confirmed `unified-1` still healthy
in ota_0 at 218,294 ms uptime. A fresh, manually initiated verify-only session
succeeded: all 500,560 bytes verified at inactive slot `0x310000`, then Abort left
the current image selected. Final status: `unified-1`, protocol 1, ota_0 at
`0x10000`, uptime 277,177 ms, OTA available. The timeout remains an observed
reliability issue; no upload operation was automatically replayed.


## No-feature USB reflash (2026-11-03)

The latest source was built in Nix without feature arguments as `ota-default-1`
and flashed over USB to the confirmed ota_0 slot. NVS/PHY and OTA metadata were
not erased. Image size: 500,688 bytes. The first link attempt failed due to a full
disk; deleting generated incremental/scratch build caches allowed the build to
finish. No source files or firmware backups were removed.

Wi-Fi status confirmed `ota-default-1`, protocol 1, OTA available, running ota_0
at `0x10000`, address 10.128.248.68, uptime 12,099 ms. The full fault/activation
suite was not rerun for this no-feature build; eleven fault probes and successful
verify-only upload were previously exercised on `unified-1`.
