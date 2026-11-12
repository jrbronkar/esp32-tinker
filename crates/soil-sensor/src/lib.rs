#![no_std]
#![deny(missing_docs)]

//! Sampling and calibration for an analog capacitive soil-moisture probe.

/// ADC adapter for the ESP32-S3 demo board wiring.
#[cfg(all(feature = "esp32s3", target_arch = "xtensa"))]
pub mod esp32s3;

/// One raw and calibrated soil-moisture sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// ADC count before filtering.
    pub raw_adc: u16,
    /// ADC count after filtering.
    pub filtered_adc: u16,
    /// Relative moisture on the calibrated 0–100% scale.
    pub moisture_percent: u8,
}

/// Holds the calibration and filter state for one probe.
pub struct SoilSensor {
    calibration: Calibration,
    smoother: Smoother,
}

impl SoilSensor {
    /// Creates a sensor with a fresh filter and probe-specific calibration.
    pub fn new(calibration: Calibration) -> Self {
        Self {
            calibration,
            smoother: Smoother::default(),
        }
    }

    /// Filters a successful ADC sample and converts it to relative moisture.
    pub fn sample(&mut self, raw_adc: u16) -> Reading {
        let filtered_adc = self.smoother.update(raw_adc);
        Reading {
            raw_adc,
            filtered_adc,
            moisture_percent: self.calibration.moisture_percent(filtered_adc),
        }
    }
}

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

/// A small integer low-pass filter for reducing ADC noise without allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Smoother {
    filtered: Option<u16>,
}

impl Smoother {
    /// Adds a sample, retaining 75% of the previous result and 25% of the new one.
    pub fn update(&mut self, raw: u16) -> u16 {
        let filtered = match self.filtered {
            None => raw,
            Some(previous) => ((u32::from(previous) * 3 + u32::from(raw)) / 4) as u16,
        };
        self.filtered = Some(filtered);
        filtered
    }
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

    #[test]
    fn reports_moisture_from_the_filtered_sample() {
        let mut sensor = SoilSensor::new(Calibration::new(4_000, 2_000).unwrap());
        assert_eq!(sensor.sample(4_000).moisture_percent, 0);
        assert_eq!(
            sensor.sample(2_000),
            Reading {
                raw_adc: 2_000,
                filtered_adc: 3_500,
                moisture_percent: 25,
            }
        );
    }

    #[test]
    fn smooths_changes_with_an_integer_low_pass_filter() {
        let mut smoother = Smoother::default();
        assert_eq!(smoother.update(4_000), 4_000);
        assert_eq!(smoother.update(3_000), 3_750);
        assert_eq!(smoother.update(3_000), 3_562);
    }
}
