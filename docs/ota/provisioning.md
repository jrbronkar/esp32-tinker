# Provisioning and recovery

Run from the repository root inside `nix develop`. Export `WIFI_SSID` and
`WIFI_PASSWORD` privately; Cargo does not load `.env`. Firmware images contain
these credentials and must stay private. Build client and firmware together.

## Initial USB setup

The backend requires the exact [16 MiB ESP32-S3 layout](../../partitions/ota-16mb.csv),
with writable, unencrypted partitions and the running app in ota_0 or ota_1:

| Partition | Offset | Size |
| --- | ---: | ---: |
| nvs | 0x9000 | 24 KiB |
| phy_init | 0xf000 | 4 KiB |
| ota_0 | 0x10000 | 3 MiB |
| ota_1 | 0x310000 | 3 MiB |
| otadata | 0x610000 | 8 KiB |

Inspect and back up the board's existing layout/data first; backups may contain
credentials. Set `SERIAL_PORT` to its USB device. Initial provisioning replaces
the partition table and clears OTA metadata; do not repeat it for ordinary updates.

```sh
FIRMWARE_VERSION=ota-a cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo
espflash flash --port "$SERIAL_PORT" --flash-size 16mb \
  --partition-table partitions/ota-16mb.csv --target-app-partition ota_0 \
  --erase-parts otadata target/xtensa-esp32s3-none-elf/release/device-demo
```

## Build and upload

```sh
nix run .#ota-image
nix run .#espctl -- tui
```

Press **U** to select an exported image. The TUI reads its embedded version
and asks for confirmation before uploading and applying the update. The image
version defaults to a Git hash. For a scripted update with a fixed version,
pass an override when exporting:

```sh
nix run .#ota-image -- ota-next
cargo run -p espctl -- upload --host BOARD_IP \
  --image target/ota/ota-next.bin --firmware-version ota-next --logs --commit
cargo run -p espctl -- status --host BOARD_IP --boot
```

Export an application-only binary, not ELF or a merged image. Its embedded version
must match `--firmware-version`. Omit `--commit` to verify without activation.
After commit, allow Wi-Fi/DHCP to reconnect and check firmware and running slot.
A missing commit ACK leaves the outcome uncertain; inspect before retrying.
See [protocol semantics](protocol.md) and [CLI options](../../tools/espctl/README.md).

## Build IMU firmware

For a board already running `imu-demo`, export an application-only OTA image:

```sh
nix run .#ota-image -- --imu
```

For a board running `device-demo`, provision the IMU firmware over USB using
the provisioning steps above and add `--features imu` to the firmware build.
OTA requires the image application to match the running application.

The IMU build expects an MPU6050 on SDA GPIO8 and SCL GPIO9. Management stays
available if the sensor is missing. Set `IMU_UDP_HOST` at build time to send
samples to the visualizer on UDP port 7778.

## Recovery

Use BOOT/RESET and USB with a known-good matching image if firmware cannot start,
join Wi-Fi or communicate with the client. Preserve OTA metadata during ordinary
USB recovery and flash the intended selected slot; writing a slot alone does not
select it. Network uploads never write the bootloader, partition table, NVS or PHY.
This is unsigned development OTA; see [production gaps](design.md#production-gaps).
