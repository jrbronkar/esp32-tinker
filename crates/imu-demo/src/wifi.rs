extern crate alloc;

use core::{fmt::Write, net::Ipv4Addr};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_hal::{
    clock::CpuClock,
    i2c::master::{Config, I2c},
    ram,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use heapless::String;
use mpu6050::{Mpu6050, ADDRESS_LOW};
use onboard_rgb::breathing_green;
use wifi_network::Settings;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const IMU_UDP_HOST: &str = env!("IMU_UDP_HOST");
const IMU_UDP_PORT: u16 = 7778;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let destination: Ipv4Addr = IMU_UDP_HOST
        .parse()
        .unwrap_or_else(|_| fatal("IMU_UDP_HOST must be an IPv4 address"));
    let stack = wifi_network::connect(
        spawner,
        peripherals.WIFI,
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        Settings {
            ssid: WIFI_SSID,
            password: WIFI_PASSWORD,
        },
    )
    .await
    .unwrap_or_else(|_| fatal("Wi-Fi telemetry initialization failed"));

    let mut telemetry = wifi_telemetry::UdpTelemetry::new(stack, destination, IMU_UDP_PORT)
        .unwrap_or_else(|_| fatal("UDP telemetry initialization failed"));

    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let i2c = I2c::new(peripherals.I2C0, i2c_config)
        .unwrap_or_else(|_| fatal("I2C peripheral configuration failed"))
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);
    let mut sensor = Mpu6050::new(i2c, ADDRESS_LOW);
    sensor
        .initialize()
        .unwrap_or_else(|_| fatal("MPU6050 initialization failed"));

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80))
        .unwrap_or_else(|_| fatal("RGB LED clock configuration failed"));
    let led_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(esp_hal::gpio::Level::Low)
        .with_idle_output(true);
    let mut led = rmt
        .channel0
        .configure_tx(&led_config)
        .map(|channel| channel.with_pin(peripherals.GPIO48))
        .unwrap_or_else(|_| fatal("RGB LED channel configuration failed"));
    let started = Instant::now();
    let mut animation_frame = 0_u16;

    println!("MPU6050 confirmed and streaming over UDP port {IMU_UDP_PORT}");
    loop {
        match sensor.read() {
            Ok(reading) => {
                let mut record = String::<160>::new();
                if write!(
                    record,
                    "IMU,{},{:.5},{:.5},{:.5},{:.3},{:.3},{:.3},{:.2}",
                    started.elapsed().as_millis(),
                    reading.acceleration_g[0],
                    reading.acceleration_g[1],
                    reading.acceleration_g[2],
                    reading.angular_velocity_dps[0],
                    reading.angular_velocity_dps[1],
                    reading.angular_velocity_dps[2],
                    reading.temperature_c,
                )
                .is_ok()
                {
                    println!("{record}");
                    if let Err(error) = telemetry.send_line(&record).await {
                        println!("UDP send failed: {error:?}");
                    }
                }
            }
            Err(error) => println!("MPU6050 read failed: {:?}", error),
        }

        let pulses = breathing_green(animation_frame);
        led = led
            .transmit(&pulses)
            .and_then(|transfer| transfer.wait())
            .unwrap_or_else(|_| fatal("RGB LED transmission failed"));
        animation_frame = animation_frame.wrapping_add(1);
        Timer::after(Duration::from_millis(20)).await;
    }
}

fn fatal(message: &str) -> ! {
    loop {
        println!("IMU firmware stopped safely: {message}");
        esp_hal::delay::Delay::new().delay_millis(3_000);
    }
}
