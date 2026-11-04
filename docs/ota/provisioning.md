# Development OTA layout and provisioning

Validated on the 16 MB ESP32-S3, MAC a4:cb:8f:d8:13:c8. The first USB migration
is complete; two subsequent Wi-Fi updates booted successfully from alternating
slots. This is the unsigned development tracer bullet, not authenticated OTA.

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

The initial board used NVS 0x9000/0x6000, PHY 0xf000/0x1000, and a factory partition
0x10000/0xfa0000. A local owner-only first-MiB backup included the entire old
477,488-byte app, bootloader, table, NVS, and PHY; its image SHA footer verified.
The backup contains embedded credentials, so it is not a shared deliverable.

## Unified-protocol migration

The earlier prototype used upload protocol 2 and marker `rust-dev-ota-v2`.
Current status and uploads share protocol 1 with marker `rust-dev-ota-v1`.
Reflash existing prototype boards over USB before using the updated uploader;
the old and new upload implementations intentionally reject one another.
The hardware results below describe the earlier build, not this migration.

## Initial provisioning (already performed on this board for the earlier build)

In nix develop with Wi-Fi variables exported:

```sh
FIRMWARE_VERSION=ota-a cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo --features dev-ota
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
FIRMWARE_VERSION=ota-next cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo --features dev-ota
mkdir -p target/ota
espflash save-image --chip esp32s3 --flash-size 16mb \
  --partition-table partitions/ota-16mb.csv --target-app-partition ota_0 \
  target/xtensa-esp32s3-none-elf/release/device-demo target/ota/ota-next.bin
```

Export an application-only .bin, not an ELF or merged image. It works in either
slot. The dev-ota build embeds a descriptor marker (`rust-dev-ota-v1`) and release
version so host/device can reject incompatible images or firmware missing the
update capability. The validated A/B/C images were 503,392 bytes each.

The image is built with your Wi-Fi credentials: keep it private. Nothing loads
`.env` automatically; plain Cargo uses exported shell variables.

## Upload, verify, then optionally activate

```sh
cargo run -p espctl -- upload --host 10.128.248.68 \
  --image target/ota/ota-next.bin --firmware-version ota-next
```

Without `--commit`, the board writes and verifies the inactive slot, then aborts
the in-memory session without activating it. A later command starts a new upload;
there is no persistent commit token or resume facility.

To upload and activate in the same session:

```sh
cargo run -p espctl -- upload --host 10.128.248.68 \
  --image target/ota/ota-next.bin --firmware-version ota-next --commit
```

Reconnect after reboot and verify version/slot:

```sh
cargo run -p espctl -- status --host 10.128.248.68 --boot
```

No request is replayed once sent. If commit's response is lost, the outcome is
uncertain: check the running version before retrying. Wi-Fi reconnection/DHCP may
take several seconds. Uploads currently take tens of seconds on this network.

## Verification and limitations

- A was provisioned by USB in ota_0. A verify-only upload of B left A running.
- Wi-Fi B commit booted from ota_1 (0x310000); B subsequently updated to C over
  Wi-Fi, which booted from ota_0 (0x10000) and still advertised OTA availability.
- Hardware failure checks passed: first-sector disconnect and wrong expected
  digest both left ota-c running without reboot. NVS readback after all tests
  matched the original 24 KiB exactly.
- Generic engine tests cover order/size, incompatible descriptors, corrupted flash,
  segment checksum/footer, and rejecting commit before verification.
- Uploads are unsigned/unencrypted and enabled only by `--features dev-ota`.
  Authentication, signed manifests, downgrade policy, power-cut fault testing,
  and reliable automatic rollback remain subsequent milestones.
- The bootloader's OTA slot selection is now verified on this board. Automatic
  rollback is not: no claim is made that this default prebuilt bootloader enables
  it. Pending-verification state is confirmed only for the actually running slot
  after Wi-Fi initialization if a rollback-enabled bootloader sets that state.
- No network operations write the bootloader, partition table, NVS, or PHY. Only
  the inactive app slot and OTA selection metadata are writable by the updater.
