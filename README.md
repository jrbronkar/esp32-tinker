# ESP32

Rust firmware experiments for two ESP32-S3 DevKitC-1 boards. The first crate is a bare-metal serial hello-world baseline.

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
