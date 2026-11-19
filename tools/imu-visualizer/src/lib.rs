//! Serial orientation parsing.

use imu_telemetry::OrientationSample;

pub fn parse_sample(line: &str) -> Option<OrientationSample> {
    let mut fields = line.trim().split(',');
    if fields.next()? != "IMU_QUAT" {
        return None;
    }
    let sample = OrientationSample {
        sample_ms: fields.next()?.parse().ok()?,
        x: fields.next()?.parse().ok()?,
        y: fields.next()?.parse().ok()?,
        z: fields.next()?.parse().ok()?,
        w: fields.next()?.parse().ok()?,
    };
    (fields.next().is_none() && sample.is_unit()).then_some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orientation_and_rejects_invalid_records() {
        let sample = OrientationSample {
            sample_ms: 5,
            w: 1.0,
            ..Default::default()
        };
        assert_eq!(parse_sample(&sample.to_string()), Some(sample));
        assert_eq!(parse_sample("IMU_QUAT,5,0,0,0,NaN"), None);
        assert_eq!(parse_sample("IMU_QUAT,5,0,0,0,0"), None);
        assert_eq!(parse_sample("MAG,5,0,0,1"), None);
    }
}
