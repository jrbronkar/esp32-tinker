use imu_telemetry::Vector3;
use imu_visualizer::{MagneticSample, Sample};

pub(super) fn decode(
    payload: &[u8],
) -> Result<(Option<Sample>, Option<MagneticSample>), imu_telemetry::Error> {
    fn vector(value: &Vector3) -> Option<[f32; 3]> {
        let values = [value.x, value.y, value.z];
        values
            .iter()
            .all(|value| value.is_finite())
            .then_some(values)
    }

    let message = imu_telemetry::decode(payload)?;
    let motion = message.motion().and_then(|motion| {
        Some(Sample {
            time_ms: motion.sample_ms,
            accel_g: vector(motion.acceleration_g()?)?,
            gyro_dps: vector(motion.angular_velocity_dps()?)?,
            temperature_c: motion
                .temperature_c
                .is_finite()
                .then_some(motion.temperature_c)?,
        })
    });
    let magnetic = message.magnetic().and_then(|magnetic| {
        Some(MagneticSample {
            time_ms: magnetic.sample_ms,
            field_ut: vector(magnetic.field_ut()?)?,
        })
    });
    Ok((motion, magnetic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use imu_telemetry::{ImuTelemetry, MagneticSample as WireMagneticSample, MotionSample};

    #[test]
    fn decodes_firmware_publication_with_both_sensors() -> Result<(), imu_telemetry::Error> {
        let message = ImuTelemetry::default()
            .init_motion(
                MotionSample::default()
                    .init_sample_ms(43)
                    .init_acceleration_g([0.0, 0.1, 1.0].into())
                    .init_angular_velocity_dps([2.0, 0.0, -1.0].into())
                    .init_temperature_c(24.0),
            )
            .init_magnetic(
                WireMagneticSample::default()
                    .init_sample_ms(41)
                    .init_field_ut([13.0, 16.0, 21.0].into()),
            );
        let payload = imu_telemetry::encode(&message)?;
        let (motion, magnetic) = decode(&payload)?;
        assert_eq!(
            motion.map(|sample| (sample.time_ms, sample.gyro_dps)),
            Some((43, [2.0, 0.0, -1.0]))
        );
        assert_eq!(
            magnetic.map(|sample| (sample.time_ms, sample.field_ut)),
            Some((41, [13.0, 16.0, 21.0]))
        );
        Ok(())
    }
}
