# IMU orientation visualizer

This macOS Rust application reads the `imu-demo` serial CSV stream and renders a live 3D outline of the ESP32/IMU assembly. A complementary filter integrates gyroscope motion and uses the accelerometer's gravity vector to correct long-term roll and pitch drift.

Telemetry parsing and orientation estimation live in `src/lib.rs`; the serial/graphics executable consumes that library. Unit tests cover parsing, gravity initialization, gyro yaw integration, duplicate timestamps, bounded data gaps, and accelerometer correction of roll drift.

## Run with both sensors over Wi-Fi

1. Mount the MPU6050 and BMM150 on the same rigid assembly and align their
   X, Y, and Z axes. Connect both to GPIO8 (SDA) and GPIO9 (SCL).
2. Set `IMU_UDP_HOST` to the viewer Mac’s LAN IPv4 address and export an IMU
   image. The firmware sends both sensor streams to this address on UDP 7778.
3. Upload the image to an OTA-provisioned board running `imu-demo`, then start
   the viewer.

```sh
export IMU_UDP_HOST=MAC_LAN_IP
nix run .#ota-image -- --imu imu-mag-demo
nix run .#espctl -- upload --host BOARD_IP \
  --image target/ota/imu-demo-imu-mag-demo.bin --firmware-version imu-mag-demo --commit
nix run .#imu-visualizer -- --udp
```

Set the Wi-Fi build credentials privately as described in the
[OTA provisioning guide](../../docs/ota/provisioning.md). Stop any other
listener on UDP 7778 before opening the viewer.

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
`MAG` records. If both appear, check `IMU_UDP_HOST` and UDP access to the
Mac. The viewer integrates gyro yaw while magnetic data is unavailable.

## Run the serial MPU6050 demo

```sh
cargo run --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p imu-demo
```

Stop the serial monitor with Ctrl+C, then launch the viewer:

```sh
nix run .#imu-visualizer -- /dev/cu.usbmodemXXXX
```

The viewer accepts `IMU,time_ms,ax,ay,az,gx,gy,gz,temp_c` and
`MAG,time_ms,x_uT,y_uT,z_uT` records on serial or UDP. The standalone serial
firmware emits only MPU6050 records.

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
