# OTA design

The development OTA path is implemented for ESP32-S3 in bare-metal Rust. Every
`device-demo` build includes status and unsigned updates over Protobuf/TCP.
Authentication, reliable rollback, and power-loss recovery remain unfinished.

## Scope and decisions

- A Mac initiates requests to an explicit board IP; no central server is needed.
- One protocol version (1) covers status and upload operations. Dispatch is by
  payload; additive schema changes use Protobuf compatibility rules.
- One shared Embassy network stack supports the management service and can also
  support telemetry. Sensor demos do not yet start the management service.
- Upload only an application image into the inactive slot. Network requests never
  rewrite the bootloader, partition table, NVS, or PHY regions.
- Validate the stored image before selecting it. Activation requires explicit
  Commit in the same verified session; verify-only is the CLI default.
- USB provisioning and physical BOOT/RESET remain the recovery path.
- gRPC clients/servers, HTTP/2 integration, and gateways are explicitly out of
  scope. Discovery, fleet rollout, a dashboard, resume and remote release fetching
  are deferred. Native gRPC would add HTTP/2/runtime integration and memory
  costs without a validated benefit for this milestone. No gRPC dependency is included.

## Architecture and ownership

```text
Mac: espctl                      ESP32-S3: device-demo
status / upload  -- Wi-Fi TCP --> device-service
                                 decode bounded Protobuf messages
                                 validate / write inactive slot
                                 read back / verify image
                                 commit boot metadata / reset
version / status <-------------  new application starts
```

| Component | Responsibility |
| --- | --- |
| `wifi-network` | Radio, connection retry, DHCP, Embassy network runner |
| `device-service` | Listener, status/upload dispatch, flash bounds, boot selection |
| `ota-protocol` and `protocol/ota.proto` | Fixed-capacity micropb messages and framing |
| `ota-core` | Portable metadata, chunk, integrity and image validation rules |
| `device-demo` | Sensor-free management firmware and application descriptor |
| `espctl` | Host validation, status, profiling and explicit upload/commit |

The runtime accepts only the supported layout and an identifiable inactive slot;
otherwise status reports OTA unavailable. The image capability marker is
`rust-dev-ota-v1`. It prevents installing incompatible firmware that removes the
update path; it is not authentication. Firmware embeds Wi-Fi credentials at
compile time, so exported images and raw flash backups are private local files.

## Update and boot lifecycle

Begin declares application/chip/version/size/digest. Sequential chunks write the
inactive slot; Finish reads flash back and checks the digest, ESP header,
descriptor, segments, checksum and image footer. Commit changes persistent
`otadata`, marks the candidate New, then software-resets.

On reset, ROM starts the flash bootloader. The bootloader reads the partition
table and boot selection, loads/maps the chosen image and jumps to its entry
point. The running application and selected next-boot application can differ.
The updater identifies the running slot independently before choosing its target.

Disconnect/Abort before commit discards volatile session state and preserves
selection; bytes already written remain in the inactive slot. Once commit starts,
a lost reply or metadata-write interruption has an uncertain outcome. Check the
running version before retrying; no transmitted operation is automatically replayed.

The confirmation hook attempts to mark a selected, actually running PendingVerify
image Valid after networking starts. It logs confirmation errors and continues; it has no
bounded health policy. Two slots and checksums do not establish automatic rollback.
See [API contracts and lifecycle diagrams](protocol.md) for exact wire behavior.

## Startup and connection failures

Wi-Fi connection attempts retry every five seconds; ordinary disconnects return
to that loop. Reported startup initialization failures are logged, followed by a
five-second delay and reset. Startup consumes peripheral handles and initializes
static resources once, so retrying it in place would need different ownership.
Permanent configuration errors require a corrected build. Other panics print a
backtrace and loop; they do not intentionally reboot. The startup-error path has
not yet been deliberately fault-injected on hardware.

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
4. Reassess TLS, discovery, a dashboard and fleet workflows after the local update
   path is reliable. Wi-Fi loss recovery and latency profiling remain follow-ups.

## Guides and evidence

- [Protocol](protocol.md): framing, operations, errors and state/sequence diagrams.
- [Provisioning](provisioning.md): layout, build/export, USB setup and upload.
- [Testing](testing.md): procedures, current evidence and gaps.
