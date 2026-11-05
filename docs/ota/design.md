# OTA design

Every `device-demo` build includes gRPC/HTTP2 management and unsigned development
updates on TCP 7780. TCP 7779 is read-only for status, profiling and log polling.
Legacy OTA is removed after feature-parity checks; USB/BOOT/RESET is recovery.

## Architecture and ownership

The Mac connects directly to the board without a central server. Protolink
owns HTTP/2/gRPC. Upload and server-streamed log RPCs share one connection.

| Component | Responsibility |
| --- | --- |
| wifi-network | Radio, reconnection, DHCP and Embassy network runner |
| device-grpc | Bounded RPC handlers, exclusive upload state, log cursors and portable inactivity watchdog |
| device-service | Embassy/TCP drivers, runtime log wakeups, inactive flash target, boot metadata and reboot |
| ota-core | Shared metadata, ordered sector writes, stored-image validation and explicit commit rules |
| ota-protocol | Fixed-capacity micropb request/response bodies |
| espctl | Host validation, gRPC uploads with optional same-connection logs, status and discovery |

Only one upload owns flash at a time. Other streams continue during network
waits; a synchronous flash write or verification can briefly delay the executor.
An abandoned upload expires after five seconds regardless of log or HTTP/2
keepalive activity. Cancellation/expiry discards volatile state and releases
flash without changing boot selection. The complete image is not held in RAM.

## Update and boot lifecycle

Begin validates application/chip/version/size/digest. Ordered 4096-byte sectors
write only the inactive slot. Finish reads flash back and verifies the digest,
ESP descriptor/version, segment checksum and image footer. Commit is explicit
and requires that verified session; it changes otadata and resets the board.
After activation begins no more writes are accepted until reset, including if
metadata update returns an error. Missing commit ACKs are uncertain outcomes;
clients never automatically replay them.

The bootloader reads the table and boot selection, maps the chosen image and
jumps to its entry point. Running and next-boot selection can differ, so the
running slot is identified independently before choosing the inactive target.
The confirmation hook attempts to mark a matching PendingVerify image Valid
after networking starts. Two slots and this hook do not prove automatic rollback.

The supported 16 MiB layout and `rust-dev-ota-v1` compatibility marker remain.
Neither authenticates firmware. Credential-bearing images stay private. Network
requests never update the bootloader, partition table, NVS or PHY regions.
See [protocol details](protocol.md), [build/upload](provisioning.md) and
[parity/validation](../device-management/protolink.md).

## Startup failures

Wi-Fi connection attempts retry after five seconds. Reported initialization
errors log, delay and reset; peripheral/static initialization is not retried in
place. Permanent configuration errors require a corrected image. Other panics
print a backtrace and loop. These startup fault paths remain unverified on hardware.

## Security plan

Current updates are unsigned, unauthenticated and unencrypted. A reachable peer
can request installation of a compatible image. SHA-256 detects corruption; it
does not authorize the sender. Secure Boot and flash encryption have not been
enabled on the development board.

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

## Remaining milestones

1. Establish rollback-enabled bootloader behavior, bounded health confirmation,
   watchdog/reset policy and power-cut fault tests.
2. Add signed releases and installation authorization, key provisioning/recovery,
   downgrade policy, and negative security tests.
3. Integrate IMU and soil firmware and verify telemetry behavior during writes.
4. Reassess TLS, a dashboard and fleet workflows after the local update
   path is reliable. Wi-Fi loss recovery and latency profiling remain follow-ups.

## Guides and evidence

- [Protocol](protocol.md): framing, operations, errors and state/sequence diagrams.
- [Provisioning](provisioning.md): layout, build/export, USB setup and upload.
- [Testing](testing.md): procedures, current evidence and gaps.

Same-LAN [device discovery](../device-management/discovery.md) is implemented as
a library-backed mDNS/DNS-SD responder sharing the network stack. Concurrent streaming management logs are implemented over gRPC.
