#![no_std]

//! Bounded Protobuf messages shared by the IMU publisher and its consumers.

use micropb::{MessageDecode, MessageEncode, PbEncoder};

#[allow(nonstandard_style, unused, irrefutable_let_patterns, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/imu_telemetry.rs"));
}
pub use generated::telemetry_::{ImuTelemetry, MagneticSample, MotionSample, Vector3};

// A full motion sample (52 bytes) and magnetic sample (30 bytes), including
// their outer tags and lengths, fit even with ten-byte uint64 timestamps.
pub type Payload = heapless::Vec<u8, 96>;

impl From<[f32; 3]> for Vector3 {
    fn from([x, y, z]: [f32; 3]) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Encode,
    Decode,
}

pub fn encode(sample: &ImuTelemetry) -> Result<Payload, Error> {
    let mut encoder = PbEncoder::new(Payload::new());
    sample.encode(&mut encoder).map_err(|_| Error::Encode)?;
    Ok(encoder.into_writer())
}

pub fn decode(data: &[u8]) -> Result<ImuTelemetry, Error> {
    let mut sample = ImuTelemetry::default();
    sample.decode_from_bytes(data).map_err(|_| Error::Decode)?;
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_both_sensors_at_max_timestamp() {
        let motion = MotionSample::default()
            .init_sample_ms(u64::MAX)
            .init_acceleration_g([0.25, -0.5, 1.0].into())
            .init_angular_velocity_dps([1.25, -42.5, 91.0].into())
            .init_temperature_c(21.5);
        let magnetic = MagneticSample::default()
            .init_sample_ms(u64::MAX - 1)
            .init_field_ut([16.0, -2.5, 40.0].into());
        let sample = ImuTelemetry::default()
            .init_motion(motion)
            .init_magnetic(magnetic);
        let payload = encode(&sample).unwrap();
        assert_eq!(decode(&payload), Ok(sample));
        assert!(decode(&payload[..payload.len() - 1]).is_err());
    }

    #[test]
    fn encodes_a_magnetic_only_publication_to_expected_wire_bytes() {
        let sample = ImuTelemetry::default().init_magnetic(
            MagneticSample::default()
                .init_sample_ms(1)
                .init_field_ut([0.0, 0.0, 1.0].into()),
        );
        assert_eq!(
            encode(&sample).unwrap().as_slice(),
            &[0x12, 9, 8, 1, 0x12, 5, 0x1d, 0, 0, 0x80, 0x3f]
        );
    }
}
