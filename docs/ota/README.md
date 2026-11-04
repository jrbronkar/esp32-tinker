# OTA documentation

The development updater uses Protobuf over TCP to write and verify an inactive
ESP32 application slot, then optionally select it for the next boot.

| Document | Purpose |
| --- | --- |
| [Design](design.md) | Architecture, decisions, milestones, and hardware evidence |
| [Protocol](protocol.md) | API contract, framing, sequence diagrams, upload states, and boot lifecycle |
| [Provisioning](provisioning.md) | Partition layout, initial USB setup, image export, upload, and recovery |
| [gRPC feasibility](grpc-feasibility.md) | Native server candidate, ESP32 compile probe, constraints, and proposed gates |
| [Testing](testing.md) | Automated checks, board fault harness, manual procedures, results, and gaps |

Commands in these guides run from the repository root inside `nix develop`.
Partition configuration stays in [`partitions/`](../../partitions), the schema in
[`protocol/ota.proto`](../../protocol/ota.proto), and CLI usage in the
[`espctl` guide](../../tools/espctl/README.md).

Uploads are unsigned and unencrypted development functionality. Authentication,
reliable rollback, and power-loss validation remain pending.
