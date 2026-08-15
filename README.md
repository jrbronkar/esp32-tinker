# ESP32

Rust firmware experiments for two ESP32-S3 DevKitC-1 boards. The first crate is a bare-metal serial hello-world baseline.

## Inventory

### Hardware

| Quantity | Item | Planned use |
| ---: | --- | --- |
| 2 | ESP32-S3 DevKitC-1 with headers | One vehicle/IMU node and one command-and-control node |
| 2 | MPU6050 accelerometer and gyroscope modules | Motion and orientation sensing over I²C |
| 2 | 830-point breadboards with power supplies | Prototyping both ESP32 nodes |
| 280 | Jumper wires | Power and signal connections from the two breadboard kits |
| 4 | Capacitive soil-moisture sensors V1.2 | Available for later ADC experiments; not part of the current drone baseline |

The hello-world baseline currently requires only one ESP32-S3 DevKitC-1 and a USB data cable connected to its USB-to-UART port.

### Software

| Component | Purpose |
| --- | --- |
| Nix flake | Pins and assembles the development environment |
| esp-rs Rust `1.95.0.0` | Compiles Rust for the ESP32-S3 Xtensa CPU |
| Espressif Xtensa GCC `15.2.0_20250920` | Links the firmware using the ESP32-S3 memory layout |
| `esp-hal` | Initializes and controls ESP32-S3 hardware |
| `esp-bootloader-esp-idf` | Adds the application descriptor required by the bootloader |
| `esp-println` | Sends formatted text through the ESP32 serial interface |
| `esp-backtrace` | Prints panic and crash diagnostics |
| `espflash` | Flashes firmware and monitors serial output |

## Safety notes

The USB-only hello-world setup is low risk, but basic electrical precautions still apply:

- Avoid unplugging the board or closing the flashing process while firmware is being written. An interruption usually leaves an incomplete image that must be flashed again.
- Use the USB-to-UART connection as the only power source during initial development. Do not simultaneously power the board through USB, the `5V` pin, or the `3V3` pin unless the power arrangement has been deliberately designed for it.
- Treat ESP32 GPIO and ADC pins as 3.3 V only. Do not connect a 5 V signal directly to them.
- Disconnect power before adding, removing, or moving breadboard wires.
- Connect all communicating devices to a shared ground before connecting their signal wires.
- Check for shorts between `3V3`, `5V`, and `GND` before applying power.
- Verify the pinout for the exact DevKitC-1 module variant before using GPIO pins. Some pins may be connected internally to flash, PSRAM, USB, or the onboard RGB LED.
- A bad firmware image may crash or repeatedly reboot, but the board can normally be recovered through its built-in ROM download mode by using the `BOOT` and `RESET` controls and flashing known-good firmware.
- Flash memory has finite write endurance. Normal development use is unlikely to approach it, but avoid unnecessary continuous reflashing.

## What runs where

The Mac builds and flashes the firmware, but it does not run the firmware. The resulting program contains Xtensa machine instructions executed by the ESP32-S3 CPU.

```text
Mac                                    ESP32-S3
---                                    --------
Nix provides the build tools
Rust cross-compiles the firmware
espflash sends it over USB  -------->  firmware is stored in SPI flash
                                       ESP32 resets and runs the firmware
terminal displays serial data <------  println! writes through UART
```

## Enter the development shell

```sh
nix develop
```

The flake pins the official esp-rs Rust compiler, Rust source component, and Espressif Xtensa linker. It does not install or modify a global Rust toolchain.

## Build

```sh
cargo build --release -p hello-world
```

This produces an ELF firmware file at:

```text
target/xtensa-esp32s3-none-elf/release/hello-world
```

The ELF contains the program, ESP32-S3 memory layout, and an ESP-IDF-compatible application descriptor used by the bootloader and flashing tools.

## Flash and monitor

Connect the ESP32-S3 DevKitC-1 over its USB-to-UART port, then run:

```sh
cargo run --release -p hello-world
```

Cargo knows how to run an ESP32-S3 binary because `.cargo/config.toml` defines this target runner:

```toml
[target.xtensa-esp32s3-none-elf]
runner = "espflash flash --monitor"
```

After compiling, `cargo run` effectively invokes:

```sh
espflash flash --monitor \
  target/xtensa-esp32s3-none-elf/release/hello-world
```

### How flashing works

`espflash`:

1. Opens the board's USB serial device.
2. Uses its reset and boot control lines to enter the ESP32 ROM download mode.
3. Speaks Espressif's serial flashing protocol to the ROM.
4. Uploads a temporary RAM-based flash stub to accelerate programming.
5. Writes the bootloader, partition table, and application into SPI flash.
6. Resets the ESP32 so it boots the new application.

SPI flash is nonvolatile, so the firmware remains installed when the board loses power.

### How serial monitoring works

The `--monitor` option keeps the serial device open after flashing. In the firmware, `esp-println` sends this message through the ESP32 UART:

```rust
println!("Hello from the ESP32-S3!");
```

The DevKitC-1's USB-to-UART bridge converts that UART output into USB serial data. `espflash` reads the data and displays it in the Mac terminal once per second.

Press `Ctrl+C` to close the monitor. This stops the monitor on the Mac; it does not stop the firmware running on the ESP32.
