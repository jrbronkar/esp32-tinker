# Capacitive soil-moisture sensor

The V1.2 capacitive probe produces an analog voltage. The ESP32-S3 samples that voltage with ADC1 on GPIO4.

## Wiring

Disconnect USB before changing wires.

| Soil sensor | ESP32-S3 |
| --- | --- |
| `VCC` | `3V3` |
| `GND` | `GND` |
| `AOUT` or `AO` | GPIO4, labeled `4` |

The MPU6050 and soil probe share 3.3 V and ground. Leave the separate breadboard power-supply module disconnected. Insert only the sensing portion into soil; keep the connector and electronics dry.

## Telemetry and calibration

The dedicated `soil-demo` firmware emits an uncalibrated reading once per second:

```text
SOIL,time_ms,raw_adc
SOIL,1000,2714
```

The raw ADC number is intentionally not presented as moisture percentage. Each probe and soil type needs two measured endpoints:

1. Record a stable reading with the clean probe in air as the dry reference.
2. Record a stable reading in representative wet soil as the wet reference.
3. Configure those values with `soil_sensor::Calibration`.

The resulting percentage is relative to those references, not a laboratory measurement of volumetric water content.

## Run

```sh
cargo run --release -p soil-demo
```
