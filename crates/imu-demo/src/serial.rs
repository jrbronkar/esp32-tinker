use core::convert::Infallible;

use esp_hal::{
    delay::Delay,
    i2c::master::{Config, ConfigError, Error as I2cError, I2c},
    main,
    peripherals::Peripherals,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use mpu6050::{Error as SensorError, Mpu6050, ADDRESS_LOW};
use onboard_rgb::breathing_green;

#[derive(Debug)]
enum StartupError {
    I2cConfiguration(ConfigError),
    LedConfiguration,
    LedTransmission,
    Sensor(SensorError<I2cError>),
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    match run(peripherals) {
        Err(error) => handle_startup_failure(error),
        Ok(never) => match never {},
    }
}

fn run(peripherals: Peripherals) -> Result<Infallible, StartupError> {
    // The MPU6050 uses I²C standard mode on the pins wired in this demo.
    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let i2c = I2c::new(peripherals.I2C0, i2c_config)
        .map_err(StartupError::I2cConfiguration)?
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80))
        .map_err(|_| StartupError::LedConfiguration)?;
    let led_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(esp_hal::gpio::Level::Low)
        .with_idle_output(true);
    let mut led = rmt
        .channel0
        .configure_tx(&led_config)
        .map_err(|_| StartupError::LedConfiguration)?
        .with_pin(peripherals.GPIO48);
    let mut sensor = Mpu6050::new(i2c, ADDRESS_LOW);
    let delay = Delay::new();

    println!("Initializing MPU6050 at I2C address 0x68...");
    sensor.initialize().map_err(StartupError::Sensor)?;
    println!("MPU6050 confirmed and awake");
    println!("IMU_CSV,time_ms,ax_g,ay_g,az_g,gx_dps,gy_dps,gz_dps,temp_c");

    let telemetry_started = Instant::now();
    let mut animation_frame = 0_u16;
    loop {
        match sensor.read() {
            Ok(reading) => {
                println!(
                    "IMU,{},{:.5},{:.5},{:.5},{:.3},{:.3},{:.3},{:.2}",
                    telemetry_started.elapsed().as_millis(),
                    reading.acceleration_g[0],
                    reading.acceleration_g[1],
                    reading.acceleration_g[2],
                    reading.angular_velocity_dps[0],
                    reading.angular_velocity_dps[1],
                    reading.angular_velocity_dps[2],
                    reading.temperature_c,
                );
            }
            Err(error) => println!("MPU6050 read failed: {:?}", error),
        }
        let pulses = breathing_green(animation_frame);
        led = led
            .transmit(&pulses)
            .map_err(|_| StartupError::LedTransmission)?
            .wait()
            .map_err(|_| StartupError::LedTransmission)?;
        animation_frame = animation_frame.wrapping_add(1);
        delay.delay_millis(20);
    }
}

fn handle_startup_failure(error: StartupError) -> ! {
    let delay = Delay::new();
    loop {
        match &error {
            StartupError::I2cConfiguration(cause) => {
                println!("I2C peripheral configuration failed: {:?}", cause)
            }
            StartupError::Sensor(cause) => println!("MPU6050 initialization failed: {:?}", cause),
            StartupError::LedConfiguration => println!("Onboard RGB LED configuration failed"),
            StartupError::LedTransmission => println!("Onboard RGB LED transmission failed"),
        }
        println!("Check wiring, then reset the ESP32 to retry initialization");
        delay.delay_millis(3_000);
    }
}
