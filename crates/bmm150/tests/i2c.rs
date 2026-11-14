use bmm150::{Bmm150, Error};
use embedded_hal::{delay::DelayNs, i2c::ErrorKind};
use embedded_hal_mock::eh1::i2c::{Mock, Transaction};

const ADDRESS: u8 = 0x13; // Exercise a breakout with both address straps high.
const TRIM: [u8; 10] = [0, 248, 0, 128, 0, 160, 0, 1, 252, 32];
// X = 128, Y = -128, Z = 50, RHALL = 4096, data ready.
const SAMPLE: [u8; 8] = [0, 4, 0, 252, 100, 0, 1, 64];

#[derive(Default)]
struct DelayLog(Vec<u32>);

impl DelayNs for DelayLog {
    fn delay_ns(&mut self, ns: u32) {
        self.0.push(ns);
    }
}

fn write(register: u8, value: u8) -> Transaction {
    Transaction::write(ADDRESS, vec![register, value])
}

fn read(register: u8, bytes: &[u8]) -> Transaction {
    Transaction::write_read(ADDRESS, vec![register], bytes.to_vec())
}

fn through_trim(trim: &[u8; 10]) -> Vec<Transaction> {
    vec![
        write(0x4B, 0x01),
        read(0x40, &[0x32]),
        write(0x4B, 0x83),
        read(0x5D, &[254, 6]),
        read(0x62, &[10, 0, 4, 248]),
        read(0x68, trim),
    ]
}

fn initialization() -> Vec<Transaction> {
    let mut transactions = through_trim(&TRIM);
    transactions.extend([write(0x51, 0x04), write(0x52, 0x0E), write(0x4C, 0x00)]);
    transactions
}

#[test]
fn initializes_and_polls_for_a_fresh_compensated_sample() {
    let mut stale = SAMPLE;
    stale[6] &= !1;
    let mut transactions = initialization();
    transactions.extend([read(0x42, &stale), read(0x42, &SAMPLE)]);
    let mut sensor = Bmm150::new(Mock::new(&transactions), ADDRESS);
    let mut delay = DelayLog::default();

    assert_eq!(sensor.try_read(), Err(Error::NotInitialized));
    assert_eq!(sensor.initialize(&mut delay), Ok(()));
    assert_eq!(delay.0, [3_000_000, 1_000_000]);
    assert_eq!(sensor.try_read(), Ok(None));
    // Reference output from Bosch's floating-point compensation equations.
    assert_eq!(sensor.try_read(), Ok(Some([44.484375, -39.15625, 48.0])));
    sensor.release().done();
}

#[test]
fn rejects_wrong_identity_before_reset() {
    let transactions = [write(0x4B, 0x01), read(0x40, &[0x31])];
    let mut sensor = Bmm150::new(Mock::new(&transactions), ADDRESS);

    assert_eq!(
        sensor.initialize(&mut DelayLog::default()),
        Err(Error::UnexpectedIdentity(0x31))
    );
    assert_eq!(sensor.try_read(), Err(Error::NotInitialized));
    sensor.release().done();
}

#[test]
fn rejects_missing_trim_before_configuring_measurements() {
    let mut sensor = Bmm150::new(Mock::new(&through_trim(&[0; 10])), ADDRESS);

    assert_eq!(
        sensor.initialize(&mut DelayLog::default()),
        Err(Error::InvalidTrim)
    );
    assert_eq!(sensor.try_read(), Err(Error::NotInitialized));
    sensor.release().done();
}

#[test]
fn reports_invalid_samples_and_bus_errors_then_resumes_reading() {
    let mut transactions = initialization();
    transactions.extend([
        read(0x42, &[0, 128, 0, 0, 0, 0, 1, 64]),   // X overflow.
        read(0x42, &[0, 0, 0, 128, 0, 0, 1, 64]),   // Y overflow.
        read(0x42, &[0, 0, 0, 0, 0, 128, 1, 64]),   // Z overflow.
        read(0x42, &[0, 4, 0, 252, 100, 0, 1, 0]),  // Zero RHALL.
        read(0x42, &[0, 4, 0, 252, 100, 0, 1, 32]), // Zero Z gain.
        read(0x42, &SAMPLE).with_error(ErrorKind::Bus),
        read(0x42, &SAMPLE),
    ]);
    let mut sensor = Bmm150::new(Mock::new(&transactions), ADDRESS);

    assert_eq!(sensor.initialize(&mut DelayLog::default()), Ok(()));
    for _ in 0..5 {
        assert_eq!(sensor.try_read(), Err(Error::InvalidSample));
    }
    assert_eq!(sensor.try_read(), Err(Error::I2c(ErrorKind::Bus)));
    assert_eq!(sensor.try_read(), Ok(Some([44.484375, -39.15625, 48.0])));
    sensor.release().done();
}

#[test]
fn failed_reinitialization_discards_old_trim_until_retry_succeeds() {
    let mut transactions = initialization();
    transactions.push(write(0x4B, 0x01).with_error(ErrorKind::Bus));
    transactions.extend(initialization());
    transactions.push(read(0x42, &SAMPLE));
    let mut sensor = Bmm150::new(Mock::new(&transactions), ADDRESS);
    let mut delay = DelayLog::default();

    assert_eq!(sensor.initialize(&mut delay), Ok(()));
    assert_eq!(
        sensor.initialize(&mut delay),
        Err(Error::I2c(ErrorKind::Bus))
    );
    assert_eq!(sensor.try_read(), Err(Error::NotInitialized));
    assert_eq!(sensor.initialize(&mut delay), Ok(()));
    assert_eq!(sensor.try_read(), Ok(Some([44.484375, -39.15625, 48.0])));
    sensor.release().done();
}
