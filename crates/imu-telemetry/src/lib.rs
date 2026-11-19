#![no_std]

//! Orientation telemetry shared by the IMU publisher and viewer.

use core::fmt;
use micropb::{MessageDecode, MessageEncode, PbEncoder};

#[cfg(feature = "fusion")]
pub mod fusion;

#[allow(nonstandard_style, unused, irrefutable_let_patterns, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/imu_telemetry.rs"));
}
pub use generated::telemetry_::{ImuTelemetry, OrientationSample};

// The nested message uses at most 33 bytes, including the outer tag and length.
pub type Payload = heapless::Vec<u8, 33>;

impl OrientationSample {
    pub fn xyzw(&self) -> [f32; 4] {
        [self.x, self.y, self.z, self.w]
    }

    pub fn is_unit(&self) -> bool {
        let norm_squared: f32 = self.xyzw().iter().map(|value| value * value).sum();
        (norm_squared - 1.0).abs() < 0.001
    }
}

impl fmt::Display for OrientationSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [x, y, z, w] = self.xyzw();
        write!(f, "IMU_QUAT,{},{x:.7},{y:.7},{z:.7},{w:.7}", self.sample_ms)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Encode,
    Decode,
    InvalidOrientation,
}

pub fn encode(orientation: &OrientationSample) -> Result<Payload, Error> {
    if !orientation.is_unit() {
        return Err(Error::InvalidOrientation);
    }
    let message = ImuTelemetry::default().init_orientation(*orientation);
    let mut encoder = PbEncoder::new(Payload::new());
    message.encode(&mut encoder).map_err(|_| Error::Encode)?;
    Ok(encoder.into_writer())
}

pub fn decode(data: &[u8]) -> Result<OrientationSample, Error> {
    let mut sample = ImuTelemetry::default();
    sample.decode_from_bytes(data).map_err(|_| Error::Decode)?;
    sample
        .orientation()
        .filter(|orientation| orientation.is_unit())
        .copied()
        .ok_or(Error::InvalidOrientation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_full_quaternion_at_max_timestamp() -> Result<(), Error> {
        let sample = OrientationSample {
            sample_ms: u64::MAX,
            x: 0.5,
            y: -0.5,
            z: 0.5,
            w: 0.5,
        };
        let payload = encode(&sample)?;
        assert_eq!(payload.len(), 33);
        assert_eq!(decode(&payload), Ok(sample));
        assert!(decode(&payload[..payload.len() - 1]).is_err());
        Ok(())
    }

    #[test]
    fn rejects_missing_and_legacy_orientation() {
        assert_eq!(decode(&[]), Err(Error::InvalidOrientation));
        assert_eq!(
            decode(&[0x12, 9, 8, 1, 0x12, 5, 0x1d, 0, 0, 0x80, 0x3f]),
            Err(Error::InvalidOrientation)
        );
        assert_eq!(
            encode(&OrientationSample::default()),
            Err(Error::InvalidOrientation)
        );
    }
}
