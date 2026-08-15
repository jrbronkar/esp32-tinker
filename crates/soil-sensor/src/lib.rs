#![no_std]
#![deny(missing_docs)]

//! Calibration helpers for an analog capacitive soil-moisture probe.

/// A calibration error that prevents meaningful percentage conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationError {
    /// Dry and wet readings are equal, so they define no usable range.
    IdenticalEndpoints,
}

/// Maps probe-specific dry and wet ADC readings onto a relative 0–100% scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    dry: u16,
    wet: u16,
}

impl Calibration {
    /// Creates a calibration from measurements in dry and wet reference media.
    pub const fn new(dry: u16, wet: u16) -> Result<Self, CalibrationError> {
        if dry == wet {
            Err(CalibrationError::IdenticalEndpoints)
        } else {
            Ok(Self { dry, wet })
        }
    }

    /// Converts an ADC reading to a clamped relative moisture percentage.
    ///
    /// This supports sensors whose ADC value either rises or falls with moisture.
    pub fn moisture_percent(self, raw: u16) -> u8 {
        let numerator = i32::from(raw) - i32::from(self.dry);
        let denominator = i32::from(self.wet) - i32::from(self.dry);
        (numerator * 100 / denominator).clamp(0, 100) as u8
    }

    /// Returns the recorded dry ADC endpoint.
    pub const fn dry(self) -> u16 {
        self.dry
    }

    /// Returns the recorded wet ADC endpoint.
    pub const fn wet(self) -> u16 {
        self.wet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_identical_endpoints() {
        assert_eq!(
            Calibration::new(2_000, 2_000),
            Err(CalibrationError::IdenticalEndpoints)
        );
    }

    #[test]
    fn converts_sensor_whose_reading_falls_when_wet() {
        let calibration = Calibration::new(3_000, 1_000).unwrap();
        assert_eq!(calibration.moisture_percent(3_000), 0);
        assert_eq!(calibration.moisture_percent(2_000), 50);
        assert_eq!(calibration.moisture_percent(1_000), 100);
    }

    #[test]
    fn converts_sensor_whose_reading_rises_when_wet() {
        let calibration = Calibration::new(1_000, 3_000).unwrap();
        assert_eq!(calibration.moisture_percent(1_000), 0);
        assert_eq!(calibration.moisture_percent(2_000), 50);
        assert_eq!(calibration.moisture_percent(3_000), 100);
    }

    #[test]
    fn clamps_readings_outside_the_calibrated_range() {
        let calibration = Calibration::new(3_000, 1_000).unwrap();
        assert_eq!(calibration.moisture_percent(4_000), 0);
        assert_eq!(calibration.moisture_percent(0), 100);
    }
}
