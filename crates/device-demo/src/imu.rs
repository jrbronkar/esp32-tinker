//! MPU6050 telemetry alongside device management on the same network.
use core::fmt::Write;

use device_service::{DeviceLogs, LogSeverity};
use embassy_net::Stack;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_hal::{
    i2c::master::{Config, I2c},
    peripherals::{GPIO8, GPIO9, I2C0},
    time::Rate,
};
use heapless::String;
use imu_demo::{Mpu6050, ADDRESS_LOW};
use wifi_telemetry::UdpTelemetry;

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
    let mut sensor = Mpu6050::new(i2c.with_sda(sda).with_scl(scl), ADDRESS_LOW);
    let mut telemetry = option_env!("IMU_UDP_HOST")
        .and_then(|host| host.parse().ok())
        .and_then(|host| UdpTelemetry::new(stack, host, 7778).ok());
    let started = Instant::now();
    let mut last_log = started;
    loop {
        if sensor.initialize().is_err() {
            logs.log(LogSeverity::Warn, "MPU6050 unavailable; check wiring");
            Timer::after_secs(5).await;
            continue;
        }
        logs.log(LogSeverity::Info, "MPU6050 ready");
        loop {
            let Ok(raw) = sensor.read_raw() else {
                logs.log(LogSeverity::Warn, "MPU6050 read failed; retrying");
                Timer::after_secs(1).await;
                break;
            };
            let reading = raw.into_reading();
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
                }
            }
            Timer::after_millis(20).await;
        }
    }
}
