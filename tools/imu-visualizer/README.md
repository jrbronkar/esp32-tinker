# IMU orientation visualizer

This macOS Rust application reads standalone IMU serial output or dual-sensor MQTT telemetry and renders a live 3D outline of the ESP32/IMU assembly. A complementary filter integrates gyroscope motion and uses the accelerometer's gravity vector to correct long-term roll and pitch drift.

Telemetry parsing and orientation estimation live in `src/lib.rs`; the serial/graphics executable consumes that library. Unit tests cover parsing, gravity initialization, gyro yaw integration, duplicate timestamps, bounded data gaps, and accelerometer correction of roll drift.

## Dual-sensor MQTT telemetry

Start an MQTT 5 broker with a TCP listener reachable from the ESP32 and
viewer. This firmware uses unauthenticated, unencrypted connections for a
trusted bench network. Set Wi-Fi credentials as described in the
[OTA provisioning guide](../../docs/ota/provisioning.md), set
`firmware.version` to `imu-mag-demo`, and add the broker’s LAN address and
a unique device ID to `firmware.yaml`:

```yaml
telemetry:
  imu:
    mqtt:
      broker: "192.0.2.10:1883"
      device_id: "esp32-01"
```

```sh
nix run .#ota-image -- --imu
nix run .#espctl -- upload --host BOARD_IP \
  --image target/ota/imu-demo-imu-mag-demo.bin --firmware-version imu-mag-demo --commit
nix run .#imu-visualizer -- --mqtt localhost esp32-01
```

The viewer accepts an optional port after the device ID. The ESP32 publishes
to `devices/<device-id>/imu` at QoS 0 without retention. Each publication
contains one binary `telemetry.ImuTelemetry` message with either or both
sensor samples. Their timestamps share the same monotonic clock and reset
when the board boots. The [Protobuf schema](../../protocol/imu_telemetry.proto)
is shared between the firmware and viewer.

To inspect one publication from the broker host:

```sh
mosquitto_sub -h localhost -V mqttv5 -t devices/esp32-01/imu -C 1 -N |
  protoc -I protocol --decode=telemetry.ImuTelemetry protocol/imu_telemetry.proto
```

## Check yaw correction

Start the viewer with the assembly still. The first valid magnetic sample
sets the relative yaw reference. The viewer uses roll and pitch to level
each magnetic vector, then moves the gyro estimate 15% of the shortest
angle toward that reference-relative magnetic direction on every fresh
sample. The overlay shows **Yaw corrected by BMM150** and the latest field
vector when correction is active.

Rotate the assembly together on a level surface, hold it still for several
seconds, and turn it back to its starting position. Watch whether yaw
settles while held still and returns near its starting value. Yaw accuracy
depends on sensor alignment and the local magnetic field. Nearby steel,
magnets, and current-carrying wires can shift the reference. Restart the
viewer to establish a new reference.

If the overlay says **Waiting for BMM150 data**, use
`espctl logs --host BOARD_IP --follow` to check for `BMM150 ready` and
`IMU MQTT connected`. Verify that the viewer and firmware use the same broker
and device ID. The viewer integrates gyro yaw while magnetic data is unavailable.

## Run the serial MPU6050 demo

```sh
cargo run --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p imu-demo
```

Stop the serial monitor with Ctrl+C, then launch the viewer:

```sh
nix run .#imu-visualizer -- /dev/cu.usbmodemXXXX
```

The standalone serial firmware emits MPU6050 records in
`IMU,time_ms,ax,ay,az,gx,gy,gz,temp_c` format.

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
