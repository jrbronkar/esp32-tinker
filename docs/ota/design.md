# OTA architecture

The Mac connects directly to the board. Upload and live logs share one HTTP/2
connection; a second client can query the same service while flash is leased.

| Component | Responsibility |
| --- | --- |
| `wifi-network` | Radio, reconnection, DHCP and network runner |
| `device-grpc` | Bounded RPC handlers, log notifications and upload watchdog |
| `device-service` | TCP/mDNS drivers, inactive flash target, boot selection and reset |
| `ota-core` | Metadata checks, ordered writes and stored-image validation |
| `ota-protocol` | Fixed-capacity Protobuf messages |
| `espctl` | Host validation, discovery and management commands |

Uploads write only the inactive application slot. Commit changes boot metadata
only after verification; no network operation writes the bootloader, partition
table, NVS or PHY regions. The running slot is identified independently of the
next-boot selection. A PendingVerify image is marked Valid only when its selected
slot is running; this does not establish automatic rollback.

## Production gaps

This is development OTA. Reachable peers can install compatible unsigned images;
SHA-256 detects corruption and does not authorize a sender. Production deployment
requires update authorization/signatures and an appropriate authenticated channel,
key provisioning/recovery, validated rollback and health deadlines, and power-cut
fault testing. Worst-case memory, executor latency, long-duration reliability and
sensor behavior during flash operations remain unverified. Synchronous flash work
can pause the executor. USB/BOOT/RESET is the recovery path.

See [protocol](protocol.md), [provisioning](provisioning.md),
[resource limits](../device-management/protolink.md) and [validation](testing.md).
