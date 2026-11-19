# IMU orientation visualizer

The viewer renders the quaternion calculated by the ESP32 from the MPU6050 and, when available, the BMM150. Both MQTT and USB serial carry the same sensor-to-reference rotation.

## MQTT orientation

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
contains one binary `telemetry.ImuTelemetry` message with a timestamp and a unit quaternion in `x, y, z, w` order. The timestamp resets when the board boots. The [Protobuf schema](../../protocol/imu_telemetry.proto) is shared by the publisher and viewer. Update the board and viewer together for this payload.

To inspect one publication from the broker host:

```sh
mosquitto_sub -h localhost -V mqttv5 -t devices/esp32-01/imu -C 1 -N |
  protoc -I protocol --decode=telemetry.ImuTelemetry protocol/imu_telemetry.proto
```

The firmware uses gravity to correct roll and pitch. The BMM150 corrects yaw relative to the first valid magnetic sample. When the magnetic field is unavailable, the firmware continues with gyro yaw, which drifts over time.

## USB serial

```sh
nix run .#imu-visualizer -- /dev/cu.usbmodemXXXX
```

The standalone firmware uses an MPU6050 and emits `IMU_QUAT,time_ms,x,y,z,w` over serial.

## Coordinate display

- Red line: sensor X axis
- Green line: sensor Y axis
- Blue line: sensor Z axis
- Sky-blue box: physical board proxy

## References

- [Macroquad 3D example](https://macroquad.rs/examples/3d/)
- [`macroquad::Camera3D` documentation](https://docs.rs/macroquad/latest/macroquad/camera/struct.Camera3D.html)
- [`serialport` Rust library](https://github.com/serialport/serialport-rs)
