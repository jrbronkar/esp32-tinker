# IMU orientation visualizer

This macOS Rust application reads the `imu-demo` serial CSV stream and renders a live 3D outline of the ESP32/IMU assembly. A complementary filter integrates gyroscope motion and uses the accelerometer's gravity vector to correct long-term roll and pitch drift.

Telemetry parsing and orientation estimation live in `src/lib.rs`; the serial/graphics executable consumes that library. Unit tests cover parsing, gravity initialization, gyro yaw integration, duplicate timestamps, bounded data gaps, and accelerometer correction of roll drift.

The MPU6050 has no magnetometer. Yaw is therefore relative, gyro-only, and expected to drift; it is displayed for transparency rather than as an absolute compass heading.

## Run

First flash the 50 Hz telemetry firmware:

```sh
cargo esp-run --release -p imu-demo
```

Press `Ctrl+C`, find the macOS call-out serial device, and launch the visualizer:

```sh
espflash monitor --list-all-ports
nix run .#imu-visualizer -- /dev/cu.usbmodemXXXX
```

Only one process can own the serial port, so close `espflash monitor` before starting the visualizer. The visualizer ignores boot and diagnostic text and parses only lines beginning with `IMU,`.

For the Wi-Fi firmware, listen directly on its default UDP port instead:

```sh
nix run .#imu-visualizer -- --udp
```

An optional second argument overrides port `7778`. Only one process can bind a UDP port at a time, so stop `nc -u -l 7778` before starting the visualizer.

`nix run` builds the application with the Nix-provided host Rust compiler rather than the older globally installed compiler.

## Coordinate display

- Red line: sensor X axis
- Green line: sensor Y axis
- Blue line: sensor Z axis
- Sky-blue box: physical board proxy

The filter is intentionally small and understandable. It is suitable for visually validating axis directions and basic orientation, not for flight control. A production estimator needs measured gyro bias, precise sample timing, vibration handling, and explicit failure behavior.

## References

- [Macroquad 3D example](https://macroquad.rs/examples/3d/)
- [`macroquad::Camera3D` documentation](https://docs.rs/macroquad/latest/macroquad/camera/struct.Camera3D.html)
- [`serialport` Rust library](https://github.com/serialport/serialport-rs)
