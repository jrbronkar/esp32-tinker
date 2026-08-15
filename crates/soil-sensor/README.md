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

The dedicated `soil-demo` firmware emits raw, smoothed, and calibrated readings once per second:

```text
SOIL,time_ms,raw_adc,filtered_adc,moisture_percent
SOIL,1000,3428,3426,100
```

The percentage is provisional because each probe and soil type needs two measured endpoints. The current firmware uses `3425` from watered soil and the ADC-saturated air reading of `4095` as its temporary dry endpoint. Replace the latter with a stable reading taken in the actual dry soil you want to use as the watering threshold.

The onboard LED breathes through the resulting color: green near the watered endpoint, yellow around the midpoint, and red near the dry endpoint. Raw and filtered ADC values remain in the serial output so calibration can be checked later.

Calibration requires:

1. Record a stable reading in representative soil at the dryness where you want the red warning.
2. Record a stable reading in the same soil after watering it normally.
3. Configure those values with `soil_sensor::Calibration`.

The resulting percentage is relative to those references, not a laboratory measurement of volumetric water content.

## Run

```sh
cargo run --release -p soil-demo
```
