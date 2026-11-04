# OTA validation and manual fault harness

This is a test plan, not a claim that every case has passed. Use a development
board with USB recovery available. Authentication and TLS are not implemented;
they cannot receive a passing security test result yet. Sensor-app integration
and rollback are also pending implementation/configuration.

## Automated tests

Inside `nix develop`, from the repo root:

```sh
cargo test \
  -p ota-core -p ota-protocol -p espctl
```

Run from the repository root. Host commands use the native compiler target;
firmware target and `build-std` flags are explicit only on firmware commands.
Tests require no board or credentials.
They cover bounded protocol decoding, fragmented/truncated TCP messages, bad
correlation and phases, request deadlines, explicit activation, lost commit reply,
ordered chunks, image corruption and injected storage read/write/commit failures.
The memory target cannot model torn flash writes or bootloader recovery.

## Board harness

Prepare a provisioned `device-demo` board and a compatible
application-only candidate binary with a distinct embedded version, following
[the provisioning guide](provisioning.md). Keep the known-good image and
partition CSV for USB recovery. Record a baseline with `espctl status --boot`.

From `nix develop` at the repository root, run one case:

```sh
cargo run \
  -p espctl --example upload-fault-probe -- \
  <board-ip> <absolute-candidate.bin> <embedded-version> disconnect
```

The probe connects directly to port 7779. It validates the candidate locally,
checks OTA availability, and uses bounded socket I/O. Ordinary failure cases assert
that the original version remains and uptime increases. Paused/manual cases print
observations only: a human must evaluate serial output and the expected outcome.
A query failure is a failed/inconclusive test, not evidence of successful recovery.

| Mode | Injection | Expected result |
| --- | --- | --- |
| `disconnect` | Close connection after first acknowledged chunk | Old firmware remains; listener accepts status; no reboot |
| `wrong-digest` | Complete upload with incorrect expected SHA-256 | Finish rejected; old firmware remains; no reboot |
| `bad-offset` | Send first chunk at offset 1 | Rejected without activation; listener recovers |
| `early-finish` | Finish before sending image bytes | Rejected; original firmware remains |
| `bad-id` | Skip request ID 2 | Rejected; original firmware remains |
| `oversized` | Initial frame length 513 | Session closes before body read; listener recovers |
| `truncated` | Half-close a truncated request body | Session closes; listener recovers |
| `bad-protobuf` | Invalid Protobuf body | Session closes; listener recovers |
| `early-commit` | Commit immediately after Begin | Rejected without activation; no reboot |
| `malformed` | Send zero frame length | Session closes; listener recovers |
| `stall` | Send half a length prefix, then wait seven seconds | Read deadline closes session; listener recovers |
| `pause-chunk` | Pause after first sector acknowledgement | Manually reset, cut power, or disable AP; inspect recovery |
| `pause-verified` | Pause after successful Finish, before Commit | Power cycle/reset must boot old app; verified state cannot survive reconnect |
| `commit-no-reply` | Send Commit without consuming reply | Outcome uncertain; inspect running version/slot after reboot |

Only `commit-no-reply` sends a valid Commit. It can activate the candidate.
`early-commit` intentionally attempts an invalid Commit that must be rejected.
No mode intentionally overwrites the bootloader, table, NVS, or PHY regions.
Upload modes still erase/write the inactive app slot.

## Physical fault and lifecycle matrix

For each run record: date, git revision, bootloader configuration/version, board,
old/candidate versions, image SHA-256, running/target offsets, injection timing,
serial log, post-test status, expected result, actual result, and PASS/FAIL/BLOCKED.
Keep credentials, firmware binaries, and raw flash backups outside tracked files.
Repeat timing-sensitive cases; one successful trial does not establish reliability.

| Condition | Procedure | Evidence / acceptance |
| --- | --- | --- |
| Normal verify-only | Run upload without `--commit`, then reset | Old version and same running slot after reset |
| Normal activation / both slots | Commit distinct versions twice | New version after each boot; offsets alternate; subsequent OTA works |
| Reset or power cut during transfer | Use `pause-chunk`; reset/cycle while paused, then Enter | Old version boots and accepts a fresh update; no automatic resume |
| Power cut during an actual sector write | Repeatedly cut power at varied times during upload, not only at the acknowledged pause | Old app recovers; fresh upload succeeds; compare preserved data; outcome unverified until exercised |
| Verified candidate abandoned | Use `pause-verified`, reset/cycle, then Enter | Old app boots; new connection cannot commit old session |
| Wi-Fi loss | At `pause-chunk`, disable AP for >5 seconds; restore AP, then Enter | After DHCP/reconnection, old app serves status and accepts fresh upload; capture failure if it does not |
| Commit reply unobserved | Use `commit-no-reply`, wait for boot, then Enter | Record actual version and slot; never infer failure solely from missing reply |
| Power cut during metadata update | Repeated `commit-no-reply` runs with external power switching at varied delays around send | Record which valid app boots, metadata, USB recovery needs; paused prompt alone cannot target the flash-write window |
| Candidate crashes / fails health | See rollback procedure below | BLOCKED until rollback-enabled bootloader and bounded health policy exist |
| Malformed wire input on device | Run malformed, stall, bad-offset, early-commit; also send oversized prefix and truncated/invalid Protobuf with a TCP tool | Connection closes or documented error; service remains reachable, original selection unchanged |
| Preserved data | Save NVS/PHY before and after fault campaign using espflash read-flash | Compare byte-for-byte when no intentional driver/data updates occurred; investigate differences |
| Sensor applications | Integrate service into IMU/soil firmware, then repeat activation and telemetry checks | BLOCKED: current sensor demos do not host the management service |
| Update authorization / encryption | Add keys, signed manifest and chosen secure transport; test wrong key, tampering, downgrade policy, identity and replay | BLOCKED: security mechanisms not implemented |

For finer malformed-input coverage, vary zero/oversized lengths, truncated prefix
and body, invalid Protobuf, unsupported versions, skipped/repeated IDs, duplicate
Begin, premature Finish, repeated Finish, and Chunk after verification. Check
liveness with a fresh status query after every rejected session. Host tests alone
do not prove the Embassy implementation handles these cases.

## Rollback and health procedure (pending)

1. Build/provision a bootloader with rollback explicitly enabled and record the
   exact configuration. Do not infer support from two slots or `New` metadata.
2. Implement a bounded health deadline and confirmation policy; the current hook
   marks a matching PendingVerify image Valid after networking and logs errors while continuing.
3. Build separate candidates that panic before network startup, hang before
   confirmation, and pass health checks. Give each a distinct version.
4. Commit each candidate, capture serial boot states, and force the reset required
   by the policy. A hang requires a watchdog/reset; rollback alone cannot reset it.
5. Require failing candidates to return to the previous version/slot and healthy
   candidates to remain selected across repeated resets. Verify another OTA works.
6. Test reset/power loss before and during health confirmation.

Do not mark rollback PASS until these checks execute on the installed bootloader.

## Result template

```text
Case / attempt:
Date / git revision / board:
Bootloader version and configuration:
Old version / candidate version / SHA-256:
Before slot / target slot:
Injection and timing:
Expected outcome:
Observed version / slot / uptime / serial evidence:
Preserved-data comparison:
Result: PASS | FAIL | BLOCKED | INCONCLUSIVE
Recovery performed / follow-up:
```

## Validation status

| Build / check | Evidence | Limits |
| --- | --- | --- |
| Current host suite | 35 tests pass: 5 discovery/client, 14 status/client, 3 upload/client, 5 core, 8 protocol | Mock I/O/storage cannot establish real flash or bootloader recovery |
| Current no-feature firmware | Firmware check and release link pass | Startup error reset path has not been fault-injected |
| `ota-merge-check-1` on board | Earlier no-feature release: 501,024 bytes uploaded, verified and committed over Wi-Fi from ota_0; post-reboot status confirms protocol 1, ota_1, OTA available | Latest activation verified; rollback and confirmation-error injection remain untested |
| `mdns-3` on board | 526,912-byte Wi-Fi activation passed; status confirms ota_0 and OTA available. Direct DNS resolves PTR/SRV/A/TXT; espctl and Bonjour have resolved the service; espctl observed committing during activation | Repeated multicast scans still intermittently miss the board, including native Bonjour in earlier tests; temporary driver/socket counters locate missed multicast queries before the IP stack; reliable LAN discovery and all cached TXT transitions remain unverified |
| `mdns-hotspot-1` network comparison | 10/10 iPhone hotspot discovery scans passed; TCP status and boot inventory passed after correcting Mac IPv4 reachability | Original WLAN remains intermittent; hotspot result does not establish the exact AP/radio cause or validate Router Alert on the wire |
| `unified-1` on board | Eleven non-activating probes and 500,560-byte verify-only upload pass | One socket timeout before a successful fresh transfer; activation/rollback not tested on this build |
| Earlier A → B → C prototype | Wi-Fi activation alternated slots; earlier NVS comparison was identical | Predates unified protocol; not evidence for every later build |

Keep detailed run logs, firmware images, and raw backups outside the repository.
Use the template above for local records and update this summary when validation
changes; distinguish new evidence from results on earlier builds.

## Remaining coverage gaps

- Actual flash/backend fault injection: partial sector writes, interruption between
  boot-selection and image-state writes, and recovery using real boot metadata.
- Rollback-enabled bootloader, crashing/hanging candidates, watchdog behavior,
  confirmation failure and a bounded health deadline.
- AP outage, DHCP renewal/address changes, and reconnecting after a broken upload.
- Persistent fuzzing, repeated malformed sessions, simultaneous clients and
  resource-exhaustion/long-duration runs on the Embassy service.
- More boundary cases: near-capacity images, malformed segment address ranges,
  oversized generated fields, duplicate operations, request ID overflow, and
  failure at later chunks/verification/abort acknowledgements.
- Alternate running slots for the expanded fault suite and additional physical
  boards/flash variants. Earlier alternating activation tests are separate evidence.
- Authentication/TLS and sensor integration remain pending features, not passing
  test cases. Paused/manual probes need an operator and recorded outcomes.

## Startup retry checks (not yet exercised on hardware)

- With the AP unavailable, check repeated connection attempts at five-second
  intervals, restore the AP, and confirm DHCP/status recover without reflashing.
- In a dedicated test build, inject a returned radio/task initialization error
  and a service-start error separately. Require the error log, five-second delay,
  reset, and a fresh startup attempt. Remove the injection and confirm recovery.
- An empty SSID is a permanent configuration error: a delayed restart is expected,
  but it cannot recover until configuration is corrected. This case does not
  prove recovery from a transient radio failure.
- Deliberately panic only in a separate diagnostic build and capture the serial
  backtrace. The current panic handler loops; startup error retry is distinct.
