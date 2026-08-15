# ESP32

Rust firmware experiments for two ESP32-S3 DevKitC-1 boards. The first crate is a bare-metal serial hello-world baseline.

## Enter the development shell

```sh
nix develop
```

The flake pins the official esp-rs Rust compiler, Rust source component, and Espressif Xtensa linker. It does not install or modify a global Rust toolchain.

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
