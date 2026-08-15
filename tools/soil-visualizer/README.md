# Soil moisture visualizer

This Rust/macOS dashboard listens for `soil-demo` UDP telemetry and displays the calibrated percentage, watering state, raw and filtered ADC values, and a rolling three-minute history.

Run it from the repository root:

```sh
tools/run-soil-visualizer
```

It listens on UDP port `7777` by default. An optional argument selects another port. Stop `nc -u -l 7777` first because only one process can bind the port.

The dashboard marks the stream stale after three seconds without a packet. Its watering labels use the project's provisional calibration and are not a plant-specific horticultural recommendation.
