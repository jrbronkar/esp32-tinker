# ESP32

Rust firmware experiments for two ESP32-S3 DevKitC-1 boards. The first crate is a bare-metal serial hello-world baseline.

## Enter the development shell

```sh
nix develop
```

On the first run, install Espressif's Xtensa Rust toolchain:

```sh
cargo install espup --locked
espup install --targets esp32s3
```

Exit and re-enter `nix develop` after installation so `~/export-esp.sh` is loaded.

## Build

```sh
cargo build --release -p hello-world
```

## Flash and monitor

Connect the ESP32-S3 DevKitC-1 over its USB-to-UART port, then run:

```sh
cargo run --release -p hello-world
```

`espflash` will select the serial device, flash the firmware, and open a monitor. The board prints `Hello, ESP32-S3!` once per second.
