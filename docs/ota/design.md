# OTA design and implementation plan

Status: unsigned development OTA tracer bullet implemented and verified on the
real ESP32-S3 (USB A -> Wi-Fi B -> Wi-Fi C, alternating slots). Authentication,
reliable automatic rollback, and power-loss testing remain pending.
Last updated: 2026-11-03.

This is the durable starting point for OTA work across sessions. Update the
decisions, milestone status, and evidence here as implementation progresses.

## Goal

Update the ESP32-S3 boards from a Mac over the existing Wi-Fi network, without
connecting USB for every firmware change. First build a tracer bullet: the
smallest working path from a newly built image on the Mac to that image running
on a board. Add authentication and other security layers after that path works.
The immediate first step is smaller: a read-only Protobuf status request from a
Mac CLI to one board and a decoded response, without any flash writes.

## Existing project

- Two ESP32-S3 DevKitC-1 boards; IMU and soil sensor applications.
- Bare-metal Rust (`no_std`), `esp-hal` 1.1.1, and
  `esp-bootloader-esp-idf` 0.5.0; this is not an ESP-IDF application.
- `crates/wifi-network` owns the shared Embassy stack, Wi-Fi, and reconnection.
  `wifi-telemetry` attaches a UDP sender to an existing stack.
- `crates/device-service` hosts read-only device status on an existing stack.
  `crates/device-demo` is the standalone sensor-free firmware for status/OTA work.
  Sensor demos use networking/telemetry and do not start the management service.
- `espflash` currently installs firmware over USB. The Nix flake pins the
  Xtensa toolchain.
- The opt-in `dev-ota` firmware now supports v2 unsigned uploads. The tested
  board is provisioned with two 3 MiB app slots and OTA metadata; status remains v1.
- Work in this checkout, `~/sources/esp32`. A separate `esp32-tinker` checkout
  was present at the same commit when this design was drafted.

## Decisions

1. The Mac initiates updates to an explicitly selected board by IP address.
   Discovery, a dashboard, cloud hosting, and automatic fleet rollout are deferred.
2. Use TCP over the existing Wi-Fi network. TCP provides an ordered byte stream;
   the application must define framing, lengths, timeouts, and responses.
   ESP-NOW is a separate future decision for sensor/control traffic.
3. Use a shared OTA firmware crate and a host-side command-line uploader. Reuse
   the existing network stack rather than initialize a second radio/stack.
   Develop management functionality in sensor-free `device-demo` first; integrate
   the proven service into sensor applications later.
4. Stream an application image into an inactive application slot. Never overwrite
   the running application, bootloader, or partition table through this updater.
5. Verify the complete stored image before changing the boot selection. Keep the
   currently selected image on transfer or verification failure.
6. Initial USB provisioning installs the OTA-capable application and partition
   layout. Physical BOOT/RESET and USB flashing remain the recovery path.
7. The tracer bullet uses plain TCP without authentication or encryption. It is
   an explicitly enabled development facility for a controlled local network.
   A hash detects corruption; it does not authorize an update.
8. Signed updates are the next milestone. TLS is deferred, not required for the
   first end-to-end demonstration. Hardware secure boot and permanent eFuse
   changes are outside this plan.
9. Use Protocol Buffers for the API contract, with a shared `.proto` schema and
   generated Rust types. Start by validating `micropb` with fixed-capacity fields
   against the pinned toolchain. micropb 0.7.0 now compiles for host and ESP32-S3.
10. Start with a small CLI for device status, then extend it for uploads. A GUI,
    dashboard, device discovery, and a separate backend are deferred. Future
    interfaces should reuse the client/protocol layer rather than introduce a
    second device API.

## Architecture

```text
Mac                                            ESP32-S3
build ELF
export application image
uploader  -------- Wi-Fi / TCP ------------->  shared OTA service
                                               validate request
                                               write inactive app slot
                                               verify stored image
          <------- result/status -----------   select next boot image
                                               reboot
          <------- version/status ----------   run new application
```

The OTA service must remain present in subsequent firmware images so that a
successful update does not remove the next update path. Compatibility metadata
identifies the application (for example, IMU versus soil) as well as the chip.

See [the API contract, sequence and lifecycle diagrams](protocol.md#operation-contract-and-ownership)
for the implemented interactions and failure semantics.

### Device API and first client

The Mac is the TCP client; the ESP32 hosts the service. No central server is
required. Protobuf defines message contents, not transport or RPC behavior; this
design does not require gRPC or HTTP.

Frame each message as a four-byte unsigned big-endian body length followed by
exactly that many Protobuf bytes. Set a small maximum frame size and reject zero
or oversized lengths before reading/decoding the body. Bound variable-length
fields in generated firmware types and handle partial reads/writes, disconnects,
and timeouts. Protocol versioning and unsupported-message behavior must be
specified in [docs/ota/protocol.md](protocol.md).

Start with `StatusRequest` and `StatusResponse`. The response should identify the
protocol version, device, application, and running firmware version. Report OTA
capability explicitly as unavailable until implemented; do not invent slot or
capacity values before the partition layout is established. Device identity in
this unsigned phase is descriptive, not cryptographically authenticated.

CLI usage (implemented; see [setup and hardware checks](../../tools/espctl/README.md)):

```sh
cargo run -p espctl -- status --host <board-ip>
```

The first client needs one explicit host, readable output, a finite timeout, and
a nonzero exit status on failure. A fleet inventory and GUI are not prerequisites.
`curl` is convenient for HTTP endpoints but is not a direct client for this
length-prefixed Protobuf TCP protocol. Do not add an HTTP service solely to enable
curl; use the small CLI to encode requests and decode responses.

Later extend the schema with begin, bounded firmware chunk, finish/verification,
commit, abort, and error messages. Stream bounded chunks rather than decode an
entire firmware image into RAM. Choose chunk and frame limits after checking RAM
use and compile results.

For later signatures, define the exact signed bytes. Protobuf serialization must
not be assumed canonical: carry and verify the original signed manifest bytes
rather than decode/re-encode a message and expect identical bytes.

### Flash and boot behavior

Plan for two application slots (`ota_0`, `ota_1`) and OTA metadata. Confirm actual
flash capacity and maximum image size before choosing offsets and slot sizes.
Determine the running slot from boot information, not merely the selected slot:
bootloader fallback can make them differ.

Use the pinned bootloader crate's partition and OTA facilities where appropriate.
Check their semantics and the actual bootloader configuration before relying on
them. Two slots alone do not provide automatic rollback after a bad application
starts. Rollback requires a compatible bootloader and explicit startup confirmation.

The first tracer bullet may demonstrate verified transfer and boot switching
before automatic rollback is implemented. State that limitation in its results.

### Transfer lifecycle

```text
idle -> inspect request -> accept -> receive -> verify -> ready to commit
                                                         |
                                                      commit
                                                         |
                                                   select + reboot
```

- A versioned, bounded request describes application/chip, release version,
  image length, and SHA-256 digest. Exact encoding is an implementation decision.
- Reject unsupported protocol versions, incompatible images, and impossible
  lengths before writing flash. Validate the ESP application image format too.
- Receive into bounded buffers; handle partial TCP reads and writes correctly.
- Bound inactivity and total transfer time. Handle one update session at a time.
- Hash the stored image, not just received bytes, before selecting it for boot.
- An explicit commit is valid only for the verified image in the current session.
- An interrupted transfer restarts from the beginning; resume is deferred.
- Once commit changes boot selection, loss of the response is ambiguous to the
  Mac. Reconnect and inspect the running version rather than assume failure.
- A rejected or disconnected session releases its resources and permits retry.

## Security progression

### First: unsigned development tracer bullet

Anyone able to reach the enabled updater can request a compatible firmware
installation. SHA-256 and compatibility checks protect against mistakes and
corruption, not a malicious sender. Keep this mode explicitly opt-in and make its
status visible. Do not describe this milestone as secure OTA.

### Next: signed releases and update authorization

- Keep a release signing private key on the Mac, outside the repository. Embed
  only its public verification key in the board firmware.
- Sign a precisely encoded manifest binding chip, application, version, length,
  and image hash. Select a maintained signature implementation after checking
  support and resource costs in the pinned Rust stack; Ed25519 is a candidate.
- Separately authorize an installation session by signing the release identity,
  board identity, fresh single-use board challenge, and requested action.
  Bind commit authorization to that same session and image.
- A release signature proves authorization of an artifact. A session challenge
  proves freshness of an installation request; these serve different purposes.
- Reject downgrades by default. A version override must be explicitly authorized;
  a `force` option must not bypass signature, chip, image, or flash bounds checks.
- Specify key provisioning, replacement, revocation, and lost-key recovery before
  treating the signing design as complete. Physical USB recovery remains available.
- Plain TCP does not authenticate the board to the Mac or hide firmware contents.
  A challenge by itself is not device authentication. Decide whether to add device
  signatures or TLS if those properties become requirements.

### Later: optional secure channel and boot enforcement

TLS protects the conversation with encryption, integrity, and endpoint
authentication when configured with trusted identities. Firmware signatures
protect the artifact across transports and storage. They are complementary.
Prefer established protocols and libraries when adding channel security.

Application-level verification cannot stop direct USB replacement of the updater.
Hardware secure boot would enforce trusted code at boot and changes recovery
assumptions. Treat it as a separate design and provisioning decision.

## Implementation milestones

### 1. Prove the status API with a CLI

- [x] Reproduce Wi-Fi firmware builds using the pinned toolchain (placeholder credentials).
- [x] Add `docs/ota/protocol.md` and a minimal `.proto` status schema with versioning,
  frame limits, field bounds, timeouts, and error semantics.
- [x] Generate Rust types using micropb and prove host/ESP32 cross-compilation.
- [x] Add a read-only TCP status listener in shared `device-service`, started by
  standalone `device-demo`; keep network, telemetry, and management independent.
- [x] Add the Mac CLI status command with readable output and useful failures.
- [x] Host-test fragmented replies, oversized lengths, malformed messages,
  disconnects, total deadlines, and reconnects against a mock TCP endpoint.
- [x] Demonstrate a real board response and recovery from malformed/partial
  requests on standalone device-demo (2026-11-03).
- [ ] Verify reconnect after actual Wi-Fi loss on hardware. Telemetry coexistence
  is deferred until management is integrated into sensor firmware.

Acceptance: run the CLI against one board's IP and observe the correct protocol,
device, application, and firmware identity. No upload, partition change, signing,
GUI, or discovery is required for this milestone.

### Follow-ups parked while OTA implementation proceeds

- [ ] Investigate remaining Wi-Fi/AP versus ESP32 driver/stack latency. Existing
  packet captures and measurements are recorded below; this is not a blocker for
  OTA. Do not optimize the network path or add persistent sessions in this phase.
- [ ] Validate reconnect after actual Wi-Fi loss.

### 2. Establish the boot and build foundations

- [x] Confirm first board flash capacity: ESP32-S3 revision v0.2, 16 MB (user-run
  `espflash board-info`, 2026-11-02 Pacific).
- [x] Read actual partition table over USB: NVS 0x9000/0x6000, PHY
  0xf000/0x1000, factory app 0x10000/0xfa0000; no OTA slots or metadata.
  Offline application size baseline: 484,880 bytes.
- [x] Validate and install `partitions/ota-16mb.csv` with two 3 MiB app slots,
  preserving NVS 0x9000/0x6000 and PHY. OTA metadata is at 0x610000.
- [x] Back up and verify the original app/table/data, provision by USB, and
  confirm all 24 KiB of NVS remain byte-identical afterward.
- [x] Export application-only images with the pinned espflash; slot 0/1 exports match.
- [x] Validate bootloader OTA selection on hardware: B booted ota_1 and C ota_0.
  Automatic rollback remains separate and unverified.
- [x] Use standalone device-demo on the confirmed 16 MB ESP32-S3 first.

### 3. Build the OTA tracer bullet

- [x] Specify v2 begin/chunk/finish/commit/abort framing and limits.
- [x] Add portable ota-core validation/state engine and a constrained flash backend.
- [x] Add Clap upload command and application-image export workflow.
- [x] Validate metadata/descriptor, stream sectors to the inactive slot, and
  read back SHA-256 plus ESP segment checksum/footer before activation.
- [x] Implement explicit commit/reboot and confirm A -> B -> C release identities.
- [x] Integrate standalone device-demo behind the explicit dev-ota feature.

Acceptance: provision once by USB, install distinguishable release B over Wi-Fi,
verify B is running, then install release C over Wi-Fi. The second update proves
that the update path survives an update. Record actual commands and serial/status
evidence. Confirm a truncated or corrupted transfer leaves the old image selected.

### 4. Make boot recovery reliable

- [ ] Build or select a confirmed rollback-enabled bootloader.
- [ ] Define bounded application health checks and explicit image confirmation.
- [ ] Keep the previous working image available during trial boot.
- [ ] Test a deliberately failing release, restart before confirmation, power loss
  during upload and activation, and physical USB recovery.
- [ ] Integrate both IMU and soil applications and check telemetry behavior during
  flash operations; document any intentional pause.

### 5. Add update authentication

- [ ] Implement key generation, public-key provisioning, and signed packages.
- [ ] Add challenge-bound authorization and authenticated commit.
- [ ] Define downgrade policy and key replacement/recovery procedures.
- [ ] Test wrong keys, altered manifests/images, stale challenges, replayed
  authorization, wrong application, oversized requests, and incomplete sessions.
- [ ] Make authenticated mode the normal path and keep unsigned development mode
  explicit and disabled by default.

### 6. Reassess additional security and convenience

- [ ] Decide whether firmware confidentiality and board authentication justify TLS.
- [ ] Consider discovery, multi-board rollout, resumable transfers, and remote
  release fetching only after the local path is reliable.
- [ ] Consider a mini dashboard/GUI for multiple-device status using the same
  client API once the CLI and OTA path are useful; no separate protocol needed.

## Verification and cross-session handoff

Host tests should exercise framing, validation, signatures, and failure handling
where meaningful. Cross-compilation checks integration but cannot establish flash,
boot, power-loss, or radio behavior; those require board tests. Record untested
hardware behavior explicitly.

At each milestone, update this file with completed checkboxes, exact reproducible
commands, observed results, and unresolved issues. Do not store Wi-Fi credentials
or private keys here.

### Current handoff: standalone device-management firmware

- Schema: `protocol/ota.proto`; framing/semantics: `docs/ota/protocol.md`.
- Shared bounded `no_std` codec/handler: `crates/ota-protocol` (micropb 0.7.0).
- Networking: `crates/wifi-network`, returning an existing stack for services.
- Listener: `crates/device-service/src/status.rs`. `device-service::start_status`
  takes a stack and application identity; no sensor or telemetry dependency.
- Release builds pass for standalone `device-demo` and both sensor Wi-Fi demos,
  using nonsecret placeholder settings. Standalone status is now verified on hardware.
- Standalone firmware: `crates/device-demo`. Requires only exported `WIFI_SSID`
  and `WIFI_PASSWORD`; `FIRMWARE_VERSION` is optional. No UDP destination required.
- Sensor demos retain their serial/Wi-Fi telemetry modes; the earlier `status`
  feature and `UdpTelemetry::start_status` have been removed.
- Host CLI/client, mock, tests, and commands: `tools/espctl`. Argument parsing uses
  Clap derives for typed options, subcommands, help, version, and range validation.
- Nix supplies tools including `protoc`. Use plain Cargo with exported variables
  inherited from the user's terminal. Python wrapper, tests, example env file,
  and Nix firmware launcher were removed at the user's request. The real local
  `.env` was left untouched and ignored; no automatic secrets loading remains.
- First board: `/dev/cu.usbmodem5C4C1816511`, ESP32-S3 revision v0.2, 40 MHz crystal,
  16 MB flash, MAC `a4:cb:8f:d8:13:c8`. User-run board-info confirms secure boot and
  flash encryption disabled. User reported the earlier soil build running, but no
  real status result was initially available; standalone device-demo is now flashed
  and its status API has been tested (see results below).
- Agent serial opens return Operation not permitted even after a device-path grant.
  Flash and monitor from the user terminal. Nix daemon access is also restricted;
  build checks use the already-installed pinned Nix-store tools directly.
- Next: verify recovery after actual Wi-Fi loss, then inspect partitions and
  design the upload/flash path. Do not change security fuses.

### Hardware status results — 2026-11-03

Board `a4:cb:8f:d8:13:c8` at `10.128.248.68:7779`, application `device-demo`,
firmware `status-v1`, protocol 1. OTA correctly reports unavailable.

- CLI command: `cargo run -p espctl -- status --host 10.128.248.68`.
- Ten consecutive queries passed and uptime increased (observed 125127 ms).
- Invalid-session recovery passed: 513-byte length prefix (`00000201`), malformed
  one-byte body (`00000001ff`), and truncated body (`0000000a0801`, then half-close).
- A stalled two-byte prefix (`0000`, connection held open) expired; another status
  query with a 7000 ms deadline succeeded after 5.06 seconds. Subsequent status
  succeeded at uptime 130877 ms. No reboot or firmware writes were performed.
- Initial immediate reconnect exposed a connection-refused gap while the sole
  socket closed/rearmed. CLI now retries only refused connection establishment
  every 50 ms within its original total deadline. It never replays a sent request.
  This is a host-only fix; no new flash is required.
- All 13 host tests pass, including a regression for a temporarily absent listener.
- Actual Wi-Fi interruption/reconnection and multiple-board testing remain untested.

## References

- [Rust on ESP: OTA](https://docs.espressif.com/projects/rust/book/application-development/ota.html)
- [Pinned bootloader crate OTA documentation](https://docs.espressif.com/projects/rust/esp-bootloader-esp-idf/0.5.0/esp32s3/esp_bootloader_esp_idf/ota/index.html)
- [ESP32-S3 OTA and rollback](https://docs.espressif.com/projects/esp-idf/en/stable/esp32s3/api-reference/system/ota.html)
- [micropb: embedded Rust Protocol Buffers](https://github.com/YuhanLiin/micropb)

The ESP-IDF guide explains bootloader concepts; its application APIs are not
directly available in this bare-metal Rust firmware.

## Latency profiling — 2026-11-03

The CLI now supports `status --timings` and `profile` (summary or per-query CSV).
The profiled client exposes timings independently of CLI presentation. Eleven
client/CLI tests pass, including controlled first-byte delay attribution and
multi-query CSV/summary checks.

Measured 50 fresh connections to the real board at 100 ms intervals: total median
34.618 ms, p95 167.049 ms, max 1010.370 ms. Loopback mock median was 0.346 ms.
Host encode/send median was 0.040 ms, decode/validate 0.0025 ms. Board connect
median was 16.612 ms; first-byte wait median 11.133 ms. Two samples experienced
11/14 refused connections, causing substantial retry and connection overhead.
The 48 no-retry samples had a total median of 33.763 ms.

Pinned esp-radio defaults to no modem power saving; it is not an established
cause. Host phases cannot distinguish radio/AP delays from device task/CPU time.
No firmware behavior was changed during these baseline runs. Next profiling step
is firmware timing around accept, read, encode, flush, and close to explain the
single-listener availability gap before considering protocol/firmware changes.

Raw samples and detailed report were saved in this chat's outputs directory as
`esp32-latency-baseline.csv`, `loopback-latency-baseline.csv`, and
`latency-profile.md`. Commands to reproduce are in `tools/espctl/README.md`.

### On-device profiling prepared

`StatusRequest.include_timings` and optional `StatusResponse.timings` are additive
v1 fields. Instrumented device-service measures request read, decode, preparation,
encoding, enqueue, ACK wait, and close using its monotonic timer. Read/decode/
prepare are returned; later stages use `STATUS_TRACE` serial logs after response
and close. Host `--timings`/`profile` request diagnostics; ordinary status does not.
Missing device fields remain explicitly unavailable on old firmware. These are
elapsed wall times, not exclusive CPU cycles or synchronized one-way network time.
Logging can affect the next listener rearm, so begin with 250 ms between queries.
Instrumented release compiles, but needs a user-terminal flash before measurements.
The detailed experiment guide is saved as `device-latency-instrumentation.md` in
this chat's outputs. No latency optimization was applied with this instrumentation.

### TCP setup investigation — 2026-11-03

30 fresh ordinary-status socket probes sampled Darwin TCP_CONNECTION_INFO.
Actual selected Mac address: 10.128.248.55 (en0 Wi-Fi), board 10.128.248.68, same
/24. Connect median was 25.685 ms including retries; 23 no-refusal queries had a
12.459 ms median connect and 11 ms median initial kernel RTT. Seven queries had
refused connects. One 1063.651 ms connect had no refused retry; SYN loss remains
a hypothesis requiring capture. Zero retransmitted-byte counts do not exclude
SYN or Wi-Fi MAC retransmission. The post-response RTT field returned a suspicious
constant 100; do not treat it as a measured 100 ms RTT.

BPF packet capture and route-socket inspection are blocked for the agent. Next:
user-terminal tcpdump filtered to this board/port plus three profile queries,
then compare SYN/SYN-ACK, data/ACK, and FIN timestamps. Detailed guide and raw data
are saved as tcp-handshake-investigation.md and tcp-kernel-profile.csv in outputs.
No radio, socket, or protocol optimizations were applied in this investigation.

### User packet capture and gateway control — 2026-11-03

Three captured SYN-to-SYN-ACK intervals: 20.024, 14.538, 8.526 ms, matching CLI
connect times within about 0.25–0.34 ms. Last request packet to response: 16.417,
19.299, 9.181 ms. Mac response ACKs: 59, 72, 64 us. No retransmissions or RSTs
visible in this short capture. Request prefix/body packets were 5–22 us apart;
each response fit one packet, so no prefix/body Nagle stall is evident. The third
teardown was truncated by the capture's 30-packet limit.

Five gateway ICMP probes afterward: 1.855–3.919 ms, average 2.647 ms, no loss.
This is an infrastructure control, not an equivalent board TCP path. The remaining
board delay cannot yet be split among AP forwarding, extra wireless hops, radio
scheduling, and device driver/stack wake-ups. Do not label it as inherent TCP cost
or assume an ACK timer from these data. The detailed report in outputs now
contains the calculations and limitations. No configuration changes were made.

## Current implementation step: read-only boot inventory

Latency work is parked as a follow-up, not an OTA prerequisite. The next build
adds `espctl status --boot`, backed by startup-only esp-storage reads before the
radio starts. It reports the configured image-header capacity, partition count,
running partition (MMU-derived), OTA slot offsets/sizes, and metadata presence.
No erase, write, boot-selection, or reboot operation has been enabled.

Candidate layout and offline export commands are documented in `docs/ota/provisioning.md`.
The baseline placeholder image exported at 484,880 bytes into either 3 MiB slot;
slot-specific exports were identical. Actual table, bootloader OTA selection, and
migration safety still need board checks. Next action is user-terminal flashing
of `boot-inspect-v1` using the existing layout, then `espctl status --boot`.
Validation: release firmware links; 20 host tests pass, including boot inventory
display and maximal bounded inventory fitting the 512-byte control frame. Do not
install the candidate table until existing data and migration are accounted for.

### Direct USB access restored — 2026-11-03

Explicit `require_escalated` serial commands now succeed under the updated
approval policy. Confirmed with board-info and a read-only 4 KiB read at 0x8000.
No firmware or partition table was written. Raw table saved in this chat's
work/current-partitions.bin. Current board uses NVS 0x9000 size 0x6000, PHY 0xf000
size 0x1000, and a factory app at 0x10000 size 0xfa0000. There are no OTA partitions.
The candidate layout currently shrinks NVS to 0x4000 and overlaps its old tail
with otadata: revise it to preserve the existing 0x6000 NVS region before any
migration. A separate later otadata region can avoid that overlap. Earlier notes
about serial access being blocked describe the previous policy; direct hardware
commands are now possible with explicit escalation. No global permission or
security-fuse changes were made by the agent.

## Current handoff: OTA tracer bullet complete — 2026-11-03

- User approved continuing implementation. USB commands now work with explicit
  escalation. Read-only snapshot of the first MiB is protected 0600 and retained
  locally at this chat's work/backup-before-ota.bin; it includes the entire old
  477,488-byte app (its ESP SHA footer verified), original table, bootloader, NVS,
  and PHY. It contains credentials and is not a shared artifact.
- Installed layout is documented in docs/ota/provisioning.md and ota-16mb.csv. The
  old 24 KiB NVS region is preserved, not shrunk. Post-test NVS read matched the
  pre-migration bytes exactly. No eFuses/security settings were changed.
- Added crates/ota-core: bounded state machine, image structure/descriptor/version
  and dev-ota capability marker, sequential sector chunks, stored-image hash,
  segment XOR checksum, ESP image SHA footer, and explicit verified-state commit.
- Device-service dev-ota is off by default. Its backend accepts only the exact
  installed layout and only the opposite of the MMU-identified running app. App
  writes cannot address bootloader/table/NVS/PHY; commit changes OTA metadata only.
- Protocol v2 uses a separate 4608-byte bound for 4096-byte chunks; v1 status keeps
  its 512-byte bound. One upload uses one connection, incremental IDs, 180-second
  total bound, and bounded I/O. No automatic request replay or resume.
- CLI: espctl upload --host IP --image FILE --firmware-version VERSION [--commit].
  Without commit it writes/verifies the inactive slot but leaves boot selection
  unchanged. Commit response loss is an uncertain outcome requiring status check.
- Real builds used the user's existing exported shell settings via nix develop,
  without printing their values. A/B/C app-only images are 503,392 bytes each and
  retained with ELFs in target/releases (git-ignored, contain Wi-Fi credentials).
- USB provisioned ota-a at 0x10000. Verify-only upload of ota-b at 0x310000 left A
  running. Commit of B booted 0x310000, then B received/committed C over Wi-Fi and C
  booted 0x10000. C reports OTA available at 10.128.248.68.
- Hardware failure checks: disconnect after first sector left C running without
  reboot; full image sent with wrong digest was rejected at Finish and C stayed
  running without reboot. Test helper: tools/espctl/examples/upload-fault-probe.rs.
- 29 host tests pass (14 status/client, 3 upload tests including acknowledgement
  rejection and lost commit reply, 5 core tests including storage fault injection,
  7 codec/framing tests). Eleven non-activating hardware fault probes and a full
  verify-only upload pass; see docs/ota/testing.md for the recorded campaign. Release dev-ota
  firmware links. Bootloader OTA selection is verified on this board; automatic
  rollback and power interruption during data/metadata writes are not verified.
- Security deliberately pending: no signatures, TLS, or downgrade enforcement.
  dev-ota enables unauthenticated writes on this LAN; ordinary builds omit it.
  Future authentication must be designed around exact signed bytes and proper
  provisioning/replacement of verification keys. Do not call this production OTA.
- Next implementation milestone: rollback-enabled bootloader plus bounded health
  confirmation and power-loss tests, then update authentication. Latency/Wi-Fi
  loss recovery investigations remain parked follow-ups.

See [OTA automated tests and manual fault harness](testing.md) for the validation
matrix, board probe modes, physical fault procedures, and result template.


### Startup failure handling

Wi-Fi association failures retry in the connection task every five seconds;
ordinary disconnects return to that loop. `device-demo` no longer panics on a
reported radio/task or management-service initialization failure: it logs the
error, waits five seconds, and software-resets to retry startup. Peripheral
ownership and one-time static initialization prevent safely rerunning the entire
initialization function in place. Permanent configuration errors still require a
corrected firmware build; rebooting does not repair them. No upload request is
replayed by this startup retry. Panics from programming errors still use the
backtrace panic handler and do not intentionally reboot.
