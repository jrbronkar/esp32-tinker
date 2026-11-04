# OTA documentation

The development updater uses Protobuf over TCP to write and verify an inactive
ESP32 application slot, then optionally select it for the next boot.

| Document | Purpose |
| --- | --- |
| [Design](design.md) | Current architecture, decisions, security plan, and remaining milestones |
| [Protocol](protocol.md) | API contract, framing, sequence diagrams, upload states, and boot lifecycle |
| [Provisioning](provisioning.md) | Partition layout, initial USB setup, image export, upload, and recovery |
| [Testing](testing.md) | Automated checks, board harness, manual procedures, validation status, and gaps |

Commands in these guides run from the repository root inside `nix develop`.
Partition configuration stays in [`partitions/`](../../partitions), the schema in
[`protocol/ota.proto`](../../protocol/ota.proto), and CLI usage in the
[`espctl` guide](../../tools/espctl/README.md).

Uploads are unsigned and unencrypted development functionality. Authentication,
reliable rollback, and power-loss validation remain pending.

## Historical reference

- [Hardware validation records](history/hardware-validation.md)
- [Implementation and latency investigation notes](history/implementation-notes.md)
- [gRPC feasibility research](history/grpc-feasibility.md) — explicitly out of scope

Historical notes preserve earlier experiments; use the guides above for current
commands and behavior.
