use esp_hal::{
    analog::adc::{Adc, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC1, GPIO4},
    Blocking,
};

use crate::{Calibration, Reading, SoilSensor};

/// Indicates that the ADC could not complete a conversion.
#[derive(Debug)]
pub struct AdcReadError;

/// Capacitive soil probe wired to GPIO4 on ADC1.
pub struct SoilProbe<'d> {
    adc: Adc<'d, ADC1<'d>, Blocking>,
    pin: AdcPin<GPIO4<'d>, ADC1<'d>>,
    sensor: SoilSensor,
}

impl<'d> SoilProbe<'d> {
    /// Configures GPIO4 at 11 dB attenuation and takes ownership of ADC1.
    pub fn new(adc: ADC1<'d>, pin: GPIO4<'d>, calibration: Calibration) -> Self {
        let mut config = AdcConfig::new();
        let pin = config.enable_pin(pin, Attenuation::_11dB);
        Self {
            adc: Adc::new(adc, config),
            pin,
            sensor: SoilSensor::new(calibration),
        }
    }

    /// Reads the ADC and updates the filter after a successful conversion.
    pub fn read(&mut self) -> Result<Reading, AdcReadError> {
        let raw = nb::block!(self.adc.read_oneshot(&mut self.pin)).map_err(|_| AdcReadError)?;
        Ok(self.sensor.sample(raw))
    }
}
