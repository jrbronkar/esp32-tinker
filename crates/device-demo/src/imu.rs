//! MPU6050 and BMM150 telemetry alongside device management on the same network.
use core::{cell::RefCell, fmt::Write};

use bmm150::{Bmm150, DEFAULT_ADDRESS};

use device_service::{DeviceLogs, LogSeverity};
use embassy_embedded_hal::shared_bus::blocking::i2c::I2cDevice;
use embassy_futures::join::join;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::NoopMutex;
use embassy_time::{Duration, Instant, Timer};
use embedded_hal::i2c::I2c as I2cBus;
use esp_hal::{
    delay::Delay,
    i2c::master::{Config, I2c},
    peripherals::{GPIO8, GPIO9, I2C0},
    time::Rate,
};
use heapless::String;
use imu_telemetry::fusion::{MagneticSample, MotionSample, OrientationEstimator};
use mpu6050::{Mpu6050, ADDRESS_LOW};

mod mqtt;

const BMM150_LAST_ADDRESS: u8 = 0x13;
const MAG_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAG_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub async fn run(
    stack: Stack<'static>,
    i2c: I2C0<'static>,
    sda: GPIO8<'static>,
    scl: GPIO9<'static>,
    logs: &'static DeviceLogs,
) -> ! {
    let config = Config::default().with_frequency(Rate::from_khz(100));
    let Ok(i2c) = I2c::new(i2c, config) else {
        logs.log(LogSeverity::Error, "IMU I2C configuration failed");
        loop {
            core::future::pending::<()>().await;
        }
    };
    // Both futures run on this executor. Each transaction releases the bus
    // before either future yields to the other.
    let bus = NoopMutex::new(RefCell::new(i2c.with_sda(sda).with_scl(scl)));
    let started = Instant::now();
    let samples = mqtt::Samples::default();
    let imu = run_mpu(I2cDevice::new(&bus), logs, started, &samples);
    let mag = run_magnetometer(I2cDevice::new(&bus), logs, started, &samples);
    let ((never, _), _) = join(join(imu, mag), mqtt::run(stack, logs, &samples)).await;
    never
}

async fn run_mpu(
    bus: impl I2cBus,
    logs: &DeviceLogs,
    started: Instant,
    samples: &mqtt::Samples,
) -> ! {
    let mut sensor = Mpu6050::new(bus, ADDRESS_LOW);
    let mut last_log = started;
    let mut estimator = OrientationEstimator::default();
    loop {
        if sensor.initialize().is_err() {
            logs.log(LogSeverity::Warn, "MPU6050 unavailable; check wiring");
            Timer::after_secs(5).await;
            continue;
        }
        logs.log(LogSeverity::Info, "MPU6050 ready");
        loop {
            let Ok(reading) = sensor.read() else {
                samples.orientation.set(None);
                logs.log(LogSeverity::Warn, "MPU6050 read failed; retrying");
                Timer::after_secs(1).await;
                break;
            };
            estimator.update(MotionSample {
                time_ms: started.elapsed().as_millis(),
                accel_g: reading.acceleration_g,
                gyro_dps: reading.angular_velocity_dps,
            });
            if let Some(field) = samples.magnetic.take() {
                estimator.correct_yaw(field);
            }
            let orientation = estimator.orientation();
            let mut record = String::<96>::new();
            if write!(record, "{orientation}").is_ok() {
                let now = Instant::now();
                if now.duration_since(last_log) >= Duration::from_secs(1) {
                    logs.log(LogSeverity::Info, &record);
                    last_log = now;
                }
            }
            samples.orientation.set(Some(orientation));
            Timer::after_millis(20).await;
        }
    }
}

async fn run_magnetometer(
    bus: impl I2cBus + Clone,
    logs: &DeviceLogs,
    started: Instant,
    samples: &mqtt::Samples,
) -> ! {
    let mut delay = Delay::new();
    loop {
        let mut detected = None;
        for address in DEFAULT_ADDRESS..=BMM150_LAST_ADDRESS {
            let mut sensor = Bmm150::new(bus.clone(), address);
            if sensor.initialize(&mut delay).is_ok() {
                detected = Some((address, sensor));
                break;
            }
        }

        if let Some((address, mut sensor)) = detected {
            let mut message = String::<64>::new();
            if write!(message, "BMM150 ready at 0x{address:02X}").is_ok() {
                logs.log(LogSeverity::Info, &message);
            }
            loop {
                match sensor.try_read() {
                    Ok(Some(field_ut)) => {
                        samples.magnetic.set(Some(MagneticSample {
                            time_ms: started.elapsed().as_millis(),
                            field_ut,
                        }));
                    }
                    Ok(None) | Err(bmm150::Error::InvalidSample) => {}
                    Err(_) => {
                        samples.magnetic.set(None);
                        logs.log(LogSeverity::Warn, "BMM150 read failed; retrying");
                        break;
                    }
                }
                Timer::after(MAG_POLL_INTERVAL).await;
            }
        } else {
            logs.log(
                LogSeverity::Warn,
                "BMM150 unavailable at 0x10–0x13; check wiring",
            );
        }
        Timer::after(MAG_RETRY_INTERVAL).await;
    }
}
