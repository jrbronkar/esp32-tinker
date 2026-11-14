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
//! ) -> Result<[f32; 3], Error<I::Error>> {
//!     let mut sensor = Bmm150::new(i2c, DEFAULT_ADDRESS);
//!     sensor.initialize(delay)?;
//!     sensor.read(delay)
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

// First register in each contiguous factory trim block.
const TRIM_X1: u8 = 0x5D;
const TRIM_Z4: u8 = 0x62;
const TRIM_Z2: u8 = 0x68;
const TRIM_XYZ1_HIGH_MASK: u8 = 0x7F;

const DATA_READY_MASK: u8 = 1 << 0;
const XY_DATA_SHIFT: u32 = 3;
const Z_DATA_SHIFT: u32 = 1;
const RHALL_DATA_SHIFT: u32 = 2;
const XY_OVERFLOW: i16 = -(1 << 12);
const Z_OVERFLOW: i16 = -(1 << 14);

const POWER_ENABLE: u8 = 0x01;
const SOFT_RESET: u8 = 0x82;
const REGULAR_REP_XY: u8 = 4; // 2 * 4 + 1 repetitions
const REGULAR_REP_Z: u8 = 14; // 14 + 1 repetitions
const NORMAL_10_HZ: u8 = 0x00;
// Startup and reset settling times used by the Bosch Sensor API.
const STARTUP_DELAY_MS: u32 = 3;
const SOFT_RESET_DELAY_MS: u32 = 1;

// Host polling cadence, independent of the sensor's output rate.
const READ_POLL_INTERVAL_MS: u32 = 10;
// Allow more than one nominal 100 ms sample period for the next fresh sample.
const READ_WAIT_MS: u32 = 150;

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
    /// No fresh sample arrived within the blocking read polling window.
    Timeout,
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
    /// preset (9 XY and 15 Z repetitions). Waits for wake-up and reset
    /// completion before accessing the chip ID and trim registers.
    pub fn initialize(&mut self, delay: &mut impl DelayNs) -> Result<(), Error<I2C::Error>> {
        self.trim = None;
        self.write(POWER_CONTROL, POWER_ENABLE)?;
        delay.delay_ms(STARTUP_DELAY_MS);

        let [id] = self.read_register(CHIP_ID)?;
        if id != EXPECTED_ID {
            return Err(Error::UnexpectedIdentity(id));
        }

        self.write(POWER_CONTROL, POWER_ENABLE | SOFT_RESET)?;
        delay.delay_ms(SOFT_RESET_DELAY_MS);

        let trim = Trim::decode(
            self.read_register(TRIM_X1)?,
            self.read_register(TRIM_Z4)?,
            self.read_register(TRIM_Z2)?,
        );
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
    /// This performs one synchronous I²C transaction with no polling delay.
    pub fn try_read(&mut self) -> Result<Option<[f32; 3]>, Error<I2C::Error>> {
        let trim = self.trim.ok_or(Error::NotInitialized)?;
        // Read the ready bit with the complete sample since reading the data
        // registers in a separate transaction can clear the flag.
        let data: [u8; 8] = self.read_register(FIELD_DATA)?;
        let [.., rhall_lsb, _] = data;
        if rhall_lsb & DATA_READY_MASK == 0 {
            return Ok(None);
        }

        trim.compensate(data).map(Some).ok_or(Error::InvalidSample)
    }

    /// Waits for a fresh field sample in sensor X, Y, Z order and microtesla.
    ///
    /// Polls immediately, then every 10 ms for up to 150 ms of delay. I²C
    /// transaction time is additional. Bus and sample errors return immediately.
    pub fn read(&mut self, delay: &mut impl DelayNs) -> Result<[f32; 3], Error<I2C::Error>> {
        for _ in 0..(READ_WAIT_MS / READ_POLL_INTERVAL_MS) {
            if let Some(field) = self.try_read()? {
                return Ok(field);
            }
            delay.delay_ms(READ_POLL_INTERVAL_MS);
        }

        self.try_read()?.ok_or(Error::Timeout)
    }

    /// Releases the I²C bus or shared-bus handle.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn read_register<const N: usize>(
        &mut self,
        register: u8,
    ) -> Result<[u8; N], Error<I2C::Error>> {
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
        let [x1, y1] = xy;
        let [z4_lsb, z4_msb, x2, y2] = z4_xy2;
        let [z2_lsb, z2_msb, z1_lsb, z1_msb, xyz1_lsb, xyz1_msb, z3_lsb, z3_msb, xy2, xy1] = mixed;
        Self {
            x1: x1 as i8,
            y1: y1 as i8,
            z4: i16::from_le_bytes([z4_lsb, z4_msb]),
            x2: x2 as i8,
            y2: y2 as i8,
            z2: i16::from_le_bytes([z2_lsb, z2_msb]),
            z1: u16::from_le_bytes([z1_lsb, z1_msb]),
            xyz1: u16::from_le_bytes([xyz1_lsb, xyz1_msb & TRIM_XYZ1_HIGH_MASK]),
            z3: i16::from_le_bytes([z3_lsb, z3_msb]),
            xy2: xy2 as i8,
            xy1,
        }
    }

    // Floating-point equations from Bosch Sensortec's BMM150 Sensor API.
    fn compensate(&self, data: [u8; 8]) -> Option<[f32; 3]> {
        const XY_HALL_GAIN_BASE: f32 = 256.0;
        const XY_AXIS_GAIN_BIAS: f32 = 160.0;
        const XY_GAIN_DIVISOR: f32 = 8192.0;
        const XY_OFFSET_SCALE: f32 = 8.0;
        const Z_HALL_GAIN_DIVISOR: f32 = 32768.0;
        const Z_OFFSET_SCALE: f32 = 131072.0;
        const Z_GAIN_SCALE: f32 = 4.0;
        const COMPENSATION_UNITS_PER_MICROTESLA: f32 = 16.0;

        let [x_lsb, x_msb, y_lsb, y_msb, z_lsb, z_msb, hall_lsb, hall_msb] = data;
        let x = i16::from_le_bytes([x_lsb, x_msb]) >> XY_DATA_SHIFT;
        let y = i16::from_le_bytes([y_lsb, y_msb]) >> XY_DATA_SHIFT;
        let z = i16::from_le_bytes([z_lsb, z_msb]) >> Z_DATA_SHIFT;
        let hall = u16::from_le_bytes([hall_lsb, hall_msb]) >> RHALL_DATA_SHIFT;

        if x == XY_OVERFLOW || y == XY_OVERFLOW || z == Z_OVERFLOW || hall == 0 {
            return None;
        }

        let hall = f32::from(hall);
        let xyz1 = f32::from(self.xyz1);
        let relative_hall = xyz1 / hall - 1.0;
        let xy_gain = XY_HALL_GAIN_BASE
            + f32::from(self.xy2) * relative_hall * relative_hall
            + f32::from(self.xy1) * relative_hall;
        let compensate_xy = |raw: i16, offset: i8, gain: i8| {
            let scaled = f32::from(raw) * xy_gain * (f32::from(gain) + XY_AXIS_GAIN_BIAS);
            (scaled / XY_GAIN_DIVISOR + f32::from(offset) * XY_OFFSET_SCALE)
                / COMPENSATION_UNITS_PER_MICROTESLA
        };

        let z_gain = f32::from(self.z2) + f32::from(self.z1) * hall / Z_HALL_GAIN_DIVISOR;
        if z_gain == 0.0 {
            return None;
        }
        let z_offset = (f32::from(z) - f32::from(self.z4)) * Z_OFFSET_SCALE;
        let z_hall = f32::from(self.z3) * (hall - xyz1);

        Some([
            compensate_xy(x, self.x1, self.x2),
            compensate_xy(y, self.y1, self.y2),
            (z_offset - z_hall) / (z_gain * (Z_GAIN_SCALE * COMPENSATION_UNITS_PER_MICROTESLA)),
        ])
    }
}
