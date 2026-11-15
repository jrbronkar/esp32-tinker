//! MPU6050 and BMM150 telemetry alongside device management on the same network.
use core::{
    cell::{Cell, RefCell},
    fmt::Write,
};

use bmm150::{Bmm150, DEFAULT_ADDRESS};

use device_service::{DeviceLogs, LogSeverity};
use embassy_embedded_hal::shared_bus::blocking::i2c::I2cDevice;
use embassy_futures::join::join;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::NoopMutex;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_hal::i2c::I2c as I2cBus;
use esp_hal::{
    delay::Delay,
    i2c::master::{Config, I2c},
    peripherals::{GPIO8, GPIO9, I2C0},
    time::Rate,
};
use heapless::String;
use mpu6050::{Mpu6050, ADDRESS_LOW};
use wifi_telemetry::UdpTelemetry;

const BMM150_LAST_ADDRESS: u8 = 0x13;
const MAG_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAG_LOG_INTERVAL: Duration = Duration::from_secs(1);
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
    let field = Cell::new(None);
    let imu = run_mpu(stack, I2cDevice::new(&bus), logs, started, &field);
    let mag = run_magnetometer(I2cDevice::new(&bus), logs, started, &field);
    let (never, _) = join(imu, mag).await;
    never
}

type MagneticSample = (u64, [f32; 3]);

async fn run_mpu(
    stack: Stack<'static>,
    bus: impl I2cBus,
    logs: &DeviceLogs,
    started: Instant,
    field: &Cell<Option<MagneticSample>>,
) -> ! {
    let mut sensor = Mpu6050::new(bus, ADDRESS_LOW);
    let mut telemetry = option_env!("IMU_UDP_HOST")
        .and_then(|host| host.parse().ok())
        .and_then(|host| UdpTelemetry::new(stack, host, 7778).ok());
    let mut last_log = started;
    loop {
        if sensor.initialize().is_err() {
            logs.log(LogSeverity::Warn, "MPU6050 unavailable; check wiring");
            Timer::after_secs(5).await;
            continue;
        }
        logs.log(LogSeverity::Info, "MPU6050 ready");
        loop {
            let Ok(reading) = sensor.read() else {
                logs.log(LogSeverity::Warn, "MPU6050 read failed; retrying");
                Timer::after_secs(1).await;
                break;
            };
            let [ax, ay, az] = reading.acceleration_g;
            let [gx, gy, gz] = reading.angular_velocity_dps;
            let temperature = reading.temperature_c;
            let mut record = String::<160>::new();
            if write!(
                record,
                "IMU,{},{ax:.5},{ay:.5},{az:.5},{gx:.3},{gy:.3},{gz:.3},{temperature:.2}",
                started.elapsed().as_millis(),
            )
            .is_ok()
            {
                let now = Instant::now();
                if now.duration_since(last_log) >= Duration::from_secs(1) {
                    logs.log(LogSeverity::Info, &record);
                    last_log = now;
                }
                if let Some(telemetry) = &mut telemetry {
                    let _ =
                        with_timeout(Duration::from_millis(20), telemetry.send_line(&record)).await;
                    if let Some((time_ms, [x, y, z])) = field.take() {
                        let mut magnetic = String::<96>::new();
                        if write!(magnetic, "MAG,{time_ms},{x:.2},{y:.2},{z:.2}").is_ok() {
                            let _ = with_timeout(
                                Duration::from_millis(20),
                                telemetry.send_line(&magnetic),
                            )
                            .await;
                        }
                    }
                }
            }
            Timer::after_millis(20).await;
        }
    }
}

async fn run_magnetometer(
    bus: impl I2cBus + Clone,
    logs: &DeviceLogs,
    started: Instant,
    field: &Cell<Option<MagneticSample>>,
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
            let mut last_log = None;
            loop {
                match sensor.try_read() {
                    Ok(Some([x, y, z])) => {
                        let now = Instant::now();
                        field.set(Some((now.duration_since(started).as_millis(), [x, y, z])));
                        if last_log.is_none_or(|last| now.duration_since(last) >= MAG_LOG_INTERVAL)
                        {
                            let mut record = String::<96>::new();
                            if write!(
                                record,
                                "MAG,{},{x:.2},{y:.2},{z:.2}",
                                now.duration_since(started).as_millis(),
                            )
                            .is_ok()
                            {
                                logs.log(LogSeverity::Info, &record);
                                last_log = Some(now);
                            }
                        }
                    }
                    Ok(None) | Err(bmm150::Error::InvalidSample) => {}
                    Err(_) => {
                        field.set(None);
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
