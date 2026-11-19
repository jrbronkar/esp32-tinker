use core::f32::consts::{PI, TAU};

use crate::OrientationSample;

/// A BMM150 field vector in microtesla, expressed in the MPU6050's axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MagneticSample {
    pub time_ms: u64,
    pub field_ut: [f32; 3],
}

fn angle_delta(target: f32, current: f32) -> f32 {
    let wrapped = (target - current + PI) % TAU;
    if wrapped < 0.0 {
        wrapped + PI
    } else {
        wrapped - PI
    }
}

/// A motion measurement from the MPU6050.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionSample {
    pub time_ms: u64,
    pub accel_g: [f32; 3],
    pub gyro_dps: [f32; 3],
}

/// Roll, pitch, and relative yaw in radians.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Orientation {
    roll: f32,
    pitch: f32,
    yaw: f32,
}

/// Complementary estimator combining gyro integration with a gravity reference.
#[derive(Clone, Copy, Debug)]
pub struct OrientationEstimator {
    orientation: Orientation,
    last_time_ms: Option<u64>,
    gyro_weight: f32,
    magnetic_reference: Option<f32>,
}

impl Default for OrientationEstimator {
    fn default() -> Self {
        Self {
            orientation: Orientation::default(),
            last_time_ms: None,
            gyro_weight: 0.98,
            magnetic_reference: None,
        }
    }
}

impl OrientationEstimator {
    /// Updates the estimate and returns its new value.
    ///
    /// Sample gaps are capped at 100 ms so a reconnect or dropped packet cannot
    /// create a single enormous rotation. Duplicate/out-of-order timestamps are ignored.
    pub fn update(&mut self, sample: MotionSample) {
        let [ax, ay, az] = sample.accel_g;
        let accel_roll = libm::atan2f(ay, az);
        let accel_pitch = libm::atan2f(-ax, libm::sqrtf(ay * ay + az * az));

        let Some(previous_ms) = self.last_time_ms else {
            self.last_time_ms = Some(sample.time_ms);
            self.orientation.roll = accel_roll;
            self.orientation.pitch = accel_pitch;
            return;
        };

        if sample.time_ms <= previous_ms {
            return;
        }
        self.last_time_ms = Some(sample.time_ms);
        let dt = ((sample.time_ms - previous_ms) as f32 / 1_000.0).min(0.1);
        let [gx, gy, gz] = sample.gyro_dps.map(f32::to_radians);

        self.orientation.roll += gx * dt;
        self.orientation.pitch += gy * dt;
        // Project body rotation onto the yaw axis when tilted. Avoid the
        // Euler singularity near vertical, where yaw cannot be separated.
        let (sin_roll, cos_roll) = libm::sincosf(self.orientation.roll);
        let cos_pitch = libm::cosf(self.orientation.pitch);
        if cos_pitch.abs() > 0.1 {
            self.orientation.yaw += (gy * sin_roll + gz * cos_roll) * dt / cos_pitch;
        }

        self.orientation.roll =
            self.gyro_weight * self.orientation.roll + (1.0 - self.gyro_weight) * accel_roll;
        self.orientation.pitch =
            self.gyro_weight * self.orientation.pitch + (1.0 - self.gyro_weight) * accel_pitch;
    }

    /// Corrects yaw once per fresh magnetic sample. The first valid field
    /// establishes a relative reference, avoiding a jump to magnetic north.
    pub fn correct_yaw(&mut self, sample: MagneticSample) -> bool {
        const MAX_SAMPLE_SKEW_MS: u64 = 250;
        const MAGNETIC_YAW_WEIGHT: f32 = 0.15;

        if !self
            .last_time_ms
            .is_some_and(|time| time.abs_diff(sample.time_ms) <= MAX_SAMPLE_SKEW_MS)
        {
            return false;
        }
        let [x, y, z] = sample.field_ut;
        let (sin_roll, cos_roll) = libm::sincosf(self.orientation.roll);
        let (sin_pitch, cos_pitch) = libm::sincosf(self.orientation.pitch);
        let horizontal_x = x * cos_pitch + (y * sin_roll + z * cos_roll) * sin_pitch;
        let horizontal_y = y * cos_roll - z * sin_roll;
        let strength = libm::hypotf(horizontal_x, horizontal_y);
        if !strength.is_finite() || strength < 1.0 {
            return false;
        }
        let heading = libm::atan2f(-horizontal_y, horizontal_x);
        let reference = self
            .magnetic_reference
            .get_or_insert(heading - self.orientation.yaw);
        self.orientation.yaw +=
            MAGNETIC_YAW_WEIGHT * angle_delta(heading - *reference, self.orientation.yaw);
        true
    }

    /// Returns the current sensor-to-reference rotation after all corrections.
    pub fn orientation(&self) -> OrientationSample {
        let (sr, cr) = libm::sincosf(self.orientation.roll * 0.5);
        let (sp, cp) = libm::sincosf(self.orientation.pitch * 0.5);
        let (sy, cy) = libm::sincosf(self.orientation.yaw * 0.5);
        OrientationSample {
            sample_ms: self.last_time_ms.unwrap_or_default(),
            x: sr * cp * cy - cr * sp * sy,
            y: cr * sp * cy + sr * cp * sy,
            z: cr * cp * sy - sr * sp * cy,
            w: cr * cp * cy + sr * sp * sy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::FRAC_PI_2;

    fn sample(time_ms: u64, accel_g: [f32; 3], gyro_dps: [f32; 3]) -> MotionSample {
        MotionSample {
            time_ms,
            accel_g,
            gyro_dps,
        }
    }

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn initializes_roll_and_pitch_from_gravity() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 1.0, 0.0], [0.0; 3]));
        let orientation = estimator.orientation;
        close(orientation.roll, FRAC_PI_2);
        close(orientation.pitch, 0.0);
        let quaternion = estimator.orientation();
        close(quaternion.x, core::f32::consts::FRAC_1_SQRT_2);
        close(quaternion.w, core::f32::consts::FRAC_1_SQRT_2);
        assert!(quaternion.is_unit());
    }

    #[test]
    fn integrates_relative_yaw_from_the_gyro() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 0.0, 1.0], [0.0; 3]));
        estimator.update(sample(20, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        let orientation = estimator.orientation;
        close(orientation.yaw.to_degrees(), 1.8);
    }

    #[test]
    fn duplicate_timestamps_do_not_integrate_rotation() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(10, [0.0, 0.0, 1.0], [0.0; 3]));
        estimator.update(sample(10, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        let orientation = estimator.orientation;
        close(orientation.yaw, 0.0);
    }

    #[test]
    fn large_timestamp_gaps_are_capped() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 0.0, 1.0], [0.0; 3]));
        estimator.update(sample(1_000, [0.0, 0.0, 1.0], [0.0, 0.0, 90.0]));
        let orientation = estimator.orientation;
        close(orientation.yaw.to_degrees(), 9.0);
    }

    #[test]
    fn magnetic_reference_corrects_stationary_gyro_drift() {
        let mut estimator = OrientationEstimator::default();
        for index in 0..100 {
            let time_ms = index * 100;
            estimator.update(sample(time_ms, [0.0, 0.0, 1.0], [0.0, 0.0, 2.0]));
            assert!(estimator.correct_yaw(MagneticSample {
                time_ms,
                field_ut: [35.0, 0.0, 20.0],
            }));
        }
        assert!(estimator.orientation.yaw.to_degrees().abs() < 2.0);
    }

    #[test]
    fn magnetic_heading_accounts_for_tilt_and_rejects_stale_data() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 1.0, 0.0], [0.0; 3]));
        assert!(estimator.correct_yaw(MagneticSample {
            time_ms: 0,
            field_ut: [35.0, 20.0, 0.0],
        }));
        let field = MagneticSample {
            time_ms: 100,
            field_ut: [0.0, 20.0, 35.0],
        };
        assert!(estimator.correct_yaw(field));
        close(estimator.orientation.yaw, FRAC_PI_2 * 0.15);
        assert!(!estimator.correct_yaw(MagneticSample {
            time_ms: 1000,
            ..field
        }));
    }

    #[test]
    fn gravity_gradually_corrects_roll_error() {
        let mut estimator = OrientationEstimator::default();
        estimator.update(sample(0, [0.0, 1.0, 0.0], [0.0; 3]));
        let initial_error = estimator.orientation.roll.abs();
        for index in 1..=100 {
            estimator.update(sample(index * 20, [0.0, 0.0, 1.0], [0.0; 3]));
        }
        assert!(estimator.orientation.roll.abs() < initial_error / 5.0);
    }
}
