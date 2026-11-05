# Development OTA layout and provisioning

For 16 MiB ESP32-S3 boards using the [partition CSV](../../partitions/ota-16mb.csv).
All commands run from the repository root inside `nix develop`, with WIFI_SSID
and WIFI_PASSWORD exported locally. Every device-demo build includes OTA.

## Installed layout

| Partition | Offset | Size |
| --- | ---: | ---: |
| nvs | 0x9000 | 24 KiB |
| phy_init | 0xf000 | 4 KiB |
| ota_0 | 0x10000 | 3 MiB |
| ota_1 | 0x310000 | 3 MiB |
| otadata | 0x610000 | 8 KiB |

`ota-16mb.csv` preserves the original NVS region, PHY region, and first app offset.
Both app slots are 64 KiB aligned. Remaining flash is unassigned. The backend
requires this exact five-entry layout with unencrypted, writable partitions,
enough configured capacity, and the running app identified as ota_0 or ota_1.
A different/invalid layout disables uploads. Running-partition identity is derived
from the MMU, not merely the OTA selection record.

## Initial USB provisioning

Inspect and back up a new board's existing layout/data before migration. This
layout preserves the tested board's original NVS/PHY regions; do not assume an
unrelated board uses those addresses. Backups may contain credentials.

In nix develop with Wi-Fi variables exported:

```sh
FIRMWARE_VERSION=ota-a cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
espflash flash --port /dev/cu.usbmodem5C4C1816511 --flash-size 16mb \
  --partition-table partitions/ota-16mb.csv --target-app-partition ota_0 \
  --erase-parts otadata target/xtensa-esp32s3-none-elf/release/device-demo
```

This intentionally changes the table, initializes only OTA metadata, and installs
the app in ota_0. Do not repeat this command for ordinary updates: it erases OTA
selection history. It preserves NVS/PHY and does not alter security fuses.
Back up a different board and inspect its data/layout before migrating it.

## Build and export the next release

```sh
FIRMWARE_VERSION=ota-next cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
mkdir -p target/ota
espflash save-image --chip esp32s3 --flash-size 16mb \
  --partition-table partitions/ota-16mb.csv --target-app-partition ota_0 \
  target/xtensa-esp32s3-none-elf/release/device-demo target/ota/ota-next.bin
```

Export an application-only .bin, not an ELF or merged image. It works in either
slot. The device-demo build embeds a descriptor marker (`rust-dev-ota-v1`) and release
version so host/device can reject incompatible images or firmware missing the
update capability.

The image is built with your Wi-Fi credentials: keep it private. Nothing loads
`.env` automatically; plain Cargo uses exported shell variables.

## Upload, verify, then optionally activate

Uploads use gRPC/HTTP2 on port **7779** by default, including the status preflight
on that same connection. Add `--logs` for concurrent server-push logs. `--port`
overrides the gRPC endpoint. The separate status/profiling commands still use
7779 through the same gRPC Management service. The legacy OTA writer and CLI fallback are removed;
there is no automatic protocol fallback or replay after an operation fails.
Build the client and firmware from the same schema. If the running device cannot
be managed by the current client, provision matching firmware over USB. Preserve
OTA metadata for ordinary USB recovery; do not repeat initial provisioning.

```sh
cargo run -p espctl -- upload --host <board-ip> \
  --image target/ota/ota-next.bin --firmware-version ota-next
```

Without `--commit`, the board writes and verifies the inactive slot, then aborts
the in-memory session without activating it. A later command starts a new upload;
there is no persistent commit token or resume facility.

To upload and activate in the same session:

```sh
cargo run -p espctl -- upload --host <board-ip> \
  --image target/ota/ota-next.bin --firmware-version ota-next --logs --commit
```

Reconnect after reboot and verify version/slot:

```sh
cargo run -p espctl -- status --host <board-ip> --boot
```

Commit is accepted only after flash readback/image verification in the same
stream. Once activation begins, further flash writes are blocked until reset.
The server allows up to two seconds for the ACK/trailers, then attempts a bounded
TCP flush and reboots; disconnects also trigger the pending reboot. Delivery
is best effort, so a lost ACK cannot prove activation failed.

No request is replayed once sent. If commit's response is lost, the outcome is
uncertain: check the running version before retrying. Wi-Fi reconnection/DHCP may
take several seconds. Uploads currently take tens of seconds on this network.

## Recovery and limitations

If the application cannot start or join Wi-Fi, use physical BOOT/RESET and USB
flashing with a known-good image. Initial provisioning clears OTA metadata;
ordinary USB reflash should preserve metadata and target the intended selected
slot. Do not blindly overwrite one slot and assume the bootloader will select it.

Uploads are unsigned and unencrypted. Reliable rollback, bounded health checks,
and power-cut recovery are not established. No network update writes the
bootloader, partition table, NVS, or PHY regions.

[Current validation and gaps](testing.md#validation-status) distinguish the latest
build from earlier activation tests. Old split-version prototypes require a USB
upgrade to unified protocol 1. After each activation, query status with `--boot`
to confirm the firmware version and running slot.
