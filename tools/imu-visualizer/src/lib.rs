//! Testable telemetry parsing and complementary orientation estimation.

/// One decoded row from the ESP32's `IMU,...` serial protocol.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub time_ms: u64,
    pub accel_g: [f32; 3],
    pub gyro_dps: [f32; 3],
    pub temperature_c: f32,
}

/// Roll, pitch, and relative yaw in radians.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Orientation {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

/// Complementary estimator combining gyro integration with a gravity reference.
#[derive(Clone, Copy, Debug)]
pub struct OrientationEstimator {
    orientation: Orientation,
    last_time_ms: Option<u64>,
    gyro_weight: f32,
}

impl Default for OrientationEstimator {
    fn default() -> Self {
        Self {
            orientation: Orientation::default(),
            last_time_ms: None,
            gyro_weight: 0.98,
        }
    }
}

impl OrientationEstimator {
    /// Updates the estimate and returns its new value.
    ///
    /// Sample gaps are capped at 100 ms so a reconnect or dropped packet cannot
    /// create a single enormous rotation. Duplicate/out-of-order timestamps use
    /// a zero integration interval.
    pub fn update(&mut self, sample: Sample) -> Orientation {
        let [ax, ay, az] = sample.accel_g;
        let accel_roll = ay.atan2(az);
        let accel_pitch = (-ax).atan2((ay * ay + az * az).sqrt());

        let Some(previous_ms) = self.last_time_ms.replace(sample.time_ms) else {
            self.orientation.roll = accel_roll;
            self.orientation.pitch = accel_pitch;
            return self.orientation;
        };

        let dt = (sample.time_ms.saturating_sub(previous_ms) as f32 / 1_000.0).min(0.1);
        let [gx, gy, gz] = sample.gyro_dps.map(f32::to_radians);

        self.orientation.roll += gx * dt;
        self.orientation.pitch += gy * dt;
        self.orientation.yaw += gz * dt;

        self.orientation.roll =
            self.gyro_weight * self.orientation.roll + (1.0 - self.gyro_weight) * accel_roll;
        self.orientation.pitch =
            self.gyro_weight * self.orientation.pitch + (1.0 - self.gyro_weight) * accel_pitch;
        self.orientation
    }

    /// Returns the latest estimate without modifying it.
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }
}

/// Parses one firmware telemetry row and ignores diagnostic/boot text.
pub fn parse_sample(line: &str) -> Option<Sample> {
    let mut fields = line.split(',');
    if fields.next()? != "IMU" {
        return None;
    }

    let sample = Sample {
        time_ms: fields.next()?.parse().ok()?,
        accel_g: [
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
        ],
        gyro_dps: [
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
        ],
        temperature_c: fields.next()?.parse().ok()?,
    };

    fields.next().is_none().then_some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    fn sample(time_ms: u64, accel_g: [f32; 3], gyro_dps: [f32; 3]) -> Sample {
        Sample {
            time_ms,
            accel_g,
            gyro_dps,
            temperature_c: 25.0,
        }
    }

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn parses_telemetry_and_ignores_other_lines() {
        assert!(parse_sample("MPU6050 confirmed and awake").is_none());
        let parsed = parse_sample("IMU,120,0.1,-0.2,0.98,1.0,-2.0,3.0,27.5").unwrap();
        assert_eq!(parsed.time_ms, 120);
        assert_eq!(parsed.accel_g, [0.1, -0.2, 0.98]);
        assert_eq!(parsed.gyro_dps, [1.0, -2.0, 3.0]);
        assert_eq!(parsed.temperature_c, 27.5);
    }

    #[test]
    fn initializes_roll_and_pitch_from_gravity() {
        let mut estimator = OrientationEstimator::default();
        let orientation = estimator.update(sample(0, [0.0, 1.0, 0.0], [0.0; 3]));
        close(orientation.roll, FRAC_PI_2);
        close(orientation.pitch, 0.0);
    }

    #[test]
    fn integrates_relative_yaw_from_the_gyro() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 0.0, 1.0], [0.0; 3]));
        let orientation = estimator.update(sample(20, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        close(orientation.yaw.to_degrees(), 1.8);
    }

    #[test]
    fn duplicate_timestamps_do_not_integrate_rotation() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(10, [0.0, 0.0, 1.0], [0.0; 3]));
        let orientation = estimator.update(sample(10, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        close(orientation.yaw, 0.0);
    }

    #[test]
    fn large_timestamp_gaps_are_capped() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 0.0, 1.0], [0.0; 3]));
        let orientation = estimator.update(sample(1_000, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        close(orientation.yaw.to_degrees(), 9.0);
    }

    #[test]
    fn gravity_gradually_corrects_roll_error() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 1.0, 0.0], [0.0; 3]));
        let initial_error = estimator.orientation().roll.abs();
        for index in 1..=100 {
            estimator.update(sample(index * 20, [0.0, 0.0, 1.0], [0.0; 3]));
        }
        assert!(estimator.orientation().roll.abs() < initial_error / 5.0);
    }
}
