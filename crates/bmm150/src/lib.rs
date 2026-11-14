#![no_std]
#![deny(missing_docs)]

//! Blocking I²C driver for the Bosch BMM150 magnetometer.
//!
//! Pass an existing ESP32 I²C bus and delay provider implementing
//! `embedded-hal` 1.0. Select the address configured by the breakout.
//!
//! ```
//! use bmm150::{Bmm150, Error, DEFAULT_ADDRESS};
//! use embedded_hal::{delay::DelayNs, i2c::I2c};
//!
//! fn read_field<I: I2c>(
//!     i2c: I,
//!     delay: &mut impl DelayNs,
//! ) -> Result<Option<[f32; 3]>, Error<I::Error>> {
//!     let mut sensor = Bmm150::new(i2c, DEFAULT_ADDRESS);
//!     sensor.initialize(delay)?;
//!     delay.delay_ms(120);
//!     sensor.try_read()
//! }
//! ```
//!
//! Keep the initialized driver when polling repeatedly.
//!
//! Initialization follows the [Bosch datasheet] sections 4.2, 5.3, 5.6, and
//! 5.8. The regular preset in table 3 specifies 9 XY repetitions and 15 Z
//! repetitions, encoded as 4 and 14 in their respective registers. The
//! separate factory trim register map and floating-point compensation come
//! from the [Bosch Sensor API].
//!
//! [Bosch datasheet]: https://www.bosch-sensortec.com/media/boschsensortec/downloads/datasheets/bst-bmm150-ds001.pdf
//! [Bosch Sensor API]: https://github.com/boschsensortec/BMM150_SensorAPI

use embedded_hal::{delay::DelayNs, i2c::I2c};

/// Default seven-bit I²C address. Address straps can select 0x11 through 0x13.
pub const DEFAULT_ADDRESS: u8 = 0x10;

const CHIP_ID: u8 = 0x40;
const EXPECTED_ID: u8 = 0x32;
const FIELD_DATA: u8 = 0x42;
const POWER_CONTROL: u8 = 0x4B;
const OP_MODE: u8 = 0x4C;
const REP_XY: u8 = 0x51;
const REP_Z: u8 = 0x52;

const POWER_ENABLE: u8 = 0x01;
const SOFT_RESET: u8 = 0x82;
const REGULAR_REP_XY: u8 = 4; // 2 * 4 + 1 repetitions
const REGULAR_REP_Z: u8 = 14; // 14 + 1 repetitions
const NORMAL_10_HZ: u8 = 0x00;

/// An error returned when initializing or reading the sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The I²C transaction failed.
    I2c(E),
    /// The device responded with the given unexpected chip ID.
    UnexpectedIdentity(u8),
    /// The factory trim is missing a value required for compensation.
    InvalidTrim,
    /// Initialization has not completed successfully.
    NotInitialized,
    /// The latest sample overflowed or could not be compensated.
    InvalidSample,
}

/// A BMM150 connected to an existing I²C bus or shared-bus handle.
pub struct Bmm150<I2C> {
    i2c: I2C,
    address: u8,
    trim: Option<Trim>,
}

impl<I2C: I2c> Bmm150<I2C> {
    /// Creates a driver with the given seven-bit address and performs no I/O.
    pub fn new(i2c: I2C, address: u8) -> Self {
        Self {
            i2c,
            address,
            trim: None,
        }
    }

    /// Wakes and verifies the sensor, then starts continuous sampling at 10 Hz.
    ///
    /// Resets the sensor to ensure all axes are enabled and uses the regular
    /// preset (9 XY and 15 Z repetitions).
    pub fn initialize(&mut self, delay: &mut impl DelayNs) -> Result<(), Error<I2C::Error>> {
        self.trim = None;
        self.write(POWER_CONTROL, POWER_ENABLE)?;
        delay.delay_ms(3);

        let [id] = self.read(CHIP_ID)?;
        if id != EXPECTED_ID {
            return Err(Error::UnexpectedIdentity(id));
        }

        self.write(POWER_CONTROL, POWER_ENABLE | SOFT_RESET)?;
        delay.delay_ms(1);

        let trim = Trim::decode(self.read(0x5D)?, self.read(0x62)?, self.read(0x68)?);
        if trim.xyz1 == 0 || trim.z1 == 0 || trim.z2 == 0 {
            return Err(Error::InvalidTrim);
        }

        self.write(REP_XY, REGULAR_REP_XY)?;
        self.write(REP_Z, REGULAR_REP_Z)?;
        self.write(OP_MODE, NORMAL_10_HZ)?;
        self.trim = Some(trim);
        Ok(())
    }

    /// Reads a fresh field sample in sensor X, Y, Z order and microtesla.
    ///
    /// Returns `Ok(None)` until new data is ready. Readout consumes the
    /// data-ready flag. Factory trim compensation is applied to valid samples.
    pub fn try_read(&mut self) -> Result<Option<[f32; 3]>, Error<I2C::Error>> {
        let trim = self.trim.ok_or(Error::NotInitialized)?;
        // Read the ready bit with the complete sample since reading the data
        // registers in a separate transaction can clear the flag.
        let data: [u8; 8] = self.read(FIELD_DATA)?;
        if data[6] & 1 == 0 {
            return Ok(None);
        }

        trim.compensate(data).map(Some).ok_or(Error::InvalidSample)
    }

    /// Releases the I²C bus or shared-bus handle.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read<const N: usize>(&mut self, register: u8) -> Result<[u8; N], Error<I2C::Error>> {
        let mut data = [0; N];
        self.i2c
            .write_read(self.address, &[register], &mut data)
            .map_err(Error::I2c)?;
        Ok(data)
    }

    fn write(&mut self, register: u8, value: u8) -> Result<(), Error<I2C::Error>> {
        self.i2c
            .write(self.address, &[register, value])
            .map_err(Error::I2c)
    }
}

#[derive(Clone, Copy)]
struct Trim {
    x1: i8,
    y1: i8,
    x2: i8,
    y2: i8,
    z1: u16,
    z2: i16,
    z3: i16,
    z4: i16,
    xy1: u8,
    xy2: i8,
    xyz1: u16,
}

impl Trim {
    fn decode(xy: [u8; 2], z4_xy2: [u8; 4], mixed: [u8; 10]) -> Self {
        Self {
            x1: xy[0] as i8,
            y1: xy[1] as i8,
            z4: i16::from_le_bytes([z4_xy2[0], z4_xy2[1]]),
            x2: z4_xy2[2] as i8,
            y2: z4_xy2[3] as i8,
            z2: i16::from_le_bytes([mixed[0], mixed[1]]),
            z1: u16::from_le_bytes([mixed[2], mixed[3]]),
            xyz1: u16::from_le_bytes([mixed[4], mixed[5] & 0x7F]),
            z3: i16::from_le_bytes([mixed[6], mixed[7]]),
            xy2: mixed[8] as i8,
            xy1: mixed[9],
        }
    }

    // Floating-point equations from Bosch Sensortec's BMM150 Sensor API.
    fn compensate(&self, data: [u8; 8]) -> Option<[f32; 3]> {
        let x = i16::from_le_bytes([data[0], data[1]]) >> 3;
        let y = i16::from_le_bytes([data[2], data[3]]) >> 3;
        let z = i16::from_le_bytes([data[4], data[5]]) >> 1;
        let hall = u16::from_le_bytes([data[6], data[7]]) >> 2;

        if x == -4096 || y == -4096 || z == -16384 || hall == 0 {
            return None;
        }

        let hall = f32::from(hall);
        let xyz1 = f32::from(self.xyz1);
        let relative_hall = xyz1 / hall - 1.0;
        let xy_gain = 256.0
            + f32::from(self.xy2) * relative_hall * relative_hall
            + f32::from(self.xy1) * relative_hall;
        let compensate_xy = |raw: i16, offset: i8, gain: i8| {
            let scaled = f32::from(raw) * xy_gain * (f32::from(gain) + 160.0);
            (scaled / 8192.0 + f32::from(offset) * 8.0) / 16.0
        };

        let z_gain = f32::from(self.z2) + f32::from(self.z1) * hall / 32768.0;
        if z_gain == 0.0 {
            return None;
        }
        let z_offset = (f32::from(z) - f32::from(self.z4)) * 131072.0;
        let z_hall = f32::from(self.z3) * (hall - xyz1);

        Some([
            compensate_xy(x, self.x1, self.x2),
            compensate_xy(y, self.y1, self.y2),
            (z_offset - z_hall) / (z_gain * 64.0),
        ])
    }
}
