#![no_std]
#![deny(missing_docs)]

//! A small, platform-independent MPU6050 driver.
//!
//! The driver depends only on the `embedded-hal` I²C trait, so the ESP32 owns
//! the concrete bus setup while register decoding and unit conversion remain
//! testable on a development computer.

use embedded_hal::i2c::I2c;

/// Default seven-bit I²C address when the MPU6050 AD0 pin is low.
pub const ADDRESS_LOW: u8 = 0x68;
/// Alternate seven-bit I²C address when the MPU6050 AD0 pin is high.
pub const ADDRESS_HIGH: u8 = 0x69;
/// Value returned by an MPU6050 from its `WHO_AM_I` register.
pub const EXPECTED_IDENTITY: u8 = 0x68;

// Register addresses from the MPU6050 register map.
const ACCEL_XOUT_H: u8 = 0x3B;
const WHO_AM_I: u8 = 0x75;
const PWR_MGMT_1: u8 = 0x6B;
const ACCEL_CONFIG: u8 = 0x1C;
const GYRO_CONFIG: u8 = 0x1B;

// Scale factors for the ranges selected in `Mpu6050::initialize`.
const ACCEL_LSB_PER_G: f32 = 16_384.0;
const GYRO_LSB_PER_DEGREE_PER_SECOND: f32 = 131.0;
const TEMPERATURE_LSB_PER_DEGREE_C: f32 = 340.0;
const TEMPERATURE_OFFSET_C: f32 = 36.53;

/// An MPU6050 operation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// Communication with the sensor failed at the I²C layer.
    I2c(E),
    /// A device responded at the address but did not identify as an MPU6050.
    UnexpectedIdentity(u8),
}

/// Unscaled signed values read directly from the MPU6050 output registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawReading {
    /// X, Y, and Z acceleration counts.
    pub acceleration: [i16; 3],
    /// Raw on-chip temperature count.
    pub temperature: i16,
    /// X, Y, and Z gyroscope counts.
    pub angular_velocity: [i16; 3],
}

/// A measurement converted into physical units using the configured ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// X, Y, and Z acceleration in multiples of standard gravity (`g`).
    pub acceleration_g: [f32; 3],
    /// On-chip temperature in degrees Celsius.
    pub temperature_c: f32,
    /// X, Y, and Z angular velocity in degrees per second.
    pub angular_velocity_dps: [f32; 3],
}

impl RawReading {
    /// Decodes the sensor's contiguous 14-byte measurement register block.
    ///
    /// The MPU6050 sends each signed 16-bit value most-significant byte first,
    /// ordered as acceleration XYZ, temperature, then gyroscope XYZ.
    pub fn from_register_bytes(bytes: [u8; 14]) -> Self {
        Self {
            acceleration: [
                signed_word(bytes[0], bytes[1]),
                signed_word(bytes[2], bytes[3]),
                signed_word(bytes[4], bytes[5]),
            ],
            temperature: signed_word(bytes[6], bytes[7]),
            angular_velocity: [
                signed_word(bytes[8], bytes[9]),
                signed_word(bytes[10], bytes[11]),
                signed_word(bytes[12], bytes[13]),
            ],
        }
    }

    /// Converts raw counts using the ±2 g and ±250 degrees/second ranges.
    pub fn into_reading(self) -> Reading {
        Reading {
            acceleration_g: self
                .acceleration
                .map(|value| value as f32 / ACCEL_LSB_PER_G),
            temperature_c: self.temperature as f32 / TEMPERATURE_LSB_PER_DEGREE_C
                + TEMPERATURE_OFFSET_C,
            angular_velocity_dps: self
                .angular_velocity
                .map(|value| value as f32 / GYRO_LSB_PER_DEGREE_PER_SECOND),
        }
    }
}

/// MPU6050 driver that owns an `embedded-hal` I²C bus.
pub struct Mpu6050<I2C> {
    i2c: I2C,
    address: u8,
}

impl<I2C> Mpu6050<I2C>
where
    I2C: I2c,
{
    /// Creates a driver for an existing I²C bus and seven-bit device address.
    pub fn new(i2c: I2C, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Verifies the device, wakes it, and selects ±2 g and ±250 dps ranges.
    pub fn initialize(&mut self) -> Result<(), Error<I2C::Error>> {
        let identity = self.identity()?;
        if identity != EXPECTED_IDENTITY {
            return Err(Error::UnexpectedIdentity(identity));
        }

        self.write_register(PWR_MGMT_1, 0x00)?; // Wake up; use the internal clock.
        self.write_register(ACCEL_CONFIG, 0x00)?; // AFS_SEL=0 selects ±2 g.
        self.write_register(GYRO_CONFIG, 0x00)?; // FS_SEL=0 selects ±250 dps.
        Ok(())
    }

    /// Reads the sensor's `WHO_AM_I` register without changing its state.
    pub fn identity(&mut self) -> Result<u8, Error<I2C::Error>> {
        let mut value = [0_u8; 1];
        self.i2c
            .write_read(self.address, &[WHO_AM_I], &mut value)
            .map_err(Error::I2c)?;
        Ok(value[0])
    }

    /// Reads one contiguous, internally consistent raw measurement frame.
    pub fn read_raw(&mut self) -> Result<RawReading, Error<I2C::Error>> {
        let mut bytes = [0_u8; 14];
        self.i2c
            .write_read(self.address, &[ACCEL_XOUT_H], &mut bytes)
            .map_err(Error::I2c)?;
        Ok(RawReading::from_register_bytes(bytes))
    }

    /// Reads one measurement frame and converts it to physical units.
    pub fn read(&mut self) -> Result<Reading, Error<I2C::Error>> {
        self.read_raw().map(RawReading::into_reading)
    }

    fn write_register(&mut self, register: u8, value: u8) -> Result<(), Error<I2C::Error>> {
        self.i2c
            .write(self.address, &[register, value])
            .map_err(Error::I2c)
    }
}

fn signed_word(high: u8, low: u8) -> i16 {
    // `from_be_bytes` also preserves the two's-complement sign for negatives.
    i16::from_be_bytes([high, low])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) {
        // Floating-point conversions should be compared within a tolerance.
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn parses_signed_big_endian_register_values() {
        let raw = RawReading::from_register_bytes([
            0x40, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x01, 0x54, 0x00, 0x83, 0xFF, 0x7D, 0x00, 0x00,
        ]);

        assert_eq!(raw.acceleration, [16_384, -16_384, 0]);
        assert_eq!(raw.temperature, 340);
        assert_eq!(raw.angular_velocity, [131, -131, 0]);
    }

    #[test]
    fn converts_default_ranges_to_physical_units() {
        let reading = RawReading {
            acceleration: [16_384, -16_384, 8_192],
            temperature: 340,
            angular_velocity: [131, -131, 262],
        }
        .into_reading();

        close(reading.acceleration_g[0], 1.0);
        close(reading.acceleration_g[1], -1.0);
        close(reading.acceleration_g[2], 0.5);
        close(reading.temperature_c, 37.53);
        close(reading.angular_velocity_dps[0], 1.0);
        close(reading.angular_velocity_dps[1], -1.0);
        close(reading.angular_velocity_dps[2], 2.0);
    }
}
