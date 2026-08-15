#![no_std]
#![no_main]

use core::convert::Infallible;

use esp_backtrace as _;
use esp_hal::{
    analog::adc::{Adc, AdcConfig, Attenuation},
    delay::Delay,
    i2c::master::{Config, ConfigError, Error as I2cError, I2c},
    main,
    peripherals::Peripherals,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use imu_demo::{Error as SensorError, Mpu6050, ADDRESS_LOW};
use onboard_rgb::breathing_green;

esp_bootloader_esp_idf::esp_app_desc!();

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

    // `run` can return only when startup fails. Once streaming begins its
    // success value is `Infallible`, which cannot actually be constructed.
    match run(peripherals) {
        Err(error) => handle_startup_failure(error),
        Ok(never) => match never {},
    }
}

fn run(peripherals: Peripherals) -> Result<Infallible, StartupError> {
    // GPIO4 samples the soil probe's 0–3.3 V analog output through ADC1.
    let mut adc_config = AdcConfig::new();
    let mut soil_pin = adc_config.enable_pin(peripherals.GPIO4, Attenuation::_11dB);
    let mut adc = Adc::new(peripherals.ADC1, adc_config);

    // Run I²C at the MPU6050's conservative standard-mode frequency of 100 kHz.
    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let i2c = I2c::new(peripherals.I2C0, i2c_config)
        .map_err(StartupError::I2cConfiguration)?
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);

    // The original DevKitC-1 routes its onboard addressable LED to GPIO48.
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

    // The breakout's disconnected AD0 pin selects the default 0x68 address.
    let mut sensor = Mpu6050::new(i2c, ADDRESS_LOW);
    let delay = Delay::new();

    println!("Initializing MPU6050 at I2C address 0x68...");

    sensor.initialize().map_err(StartupError::Sensor)?;

    println!("MPU6050 confirmed and awake");
    println!("IMU_CSV,time_ms,ax_g,ay_g,az_g,gx_dps,gy_dps,gz_dps,temp_c");

    let telemetry_started = Instant::now();
    let mut animation_frame = 0_u16;
    let mut soil_frame = 0_u8;

    loop {
        // Read all seven sensor outputs in one I²C transaction so the axes
        // belong to the same measurement frame.
        match sensor.read_raw() {
            Ok(raw) => {
                // Conversion is pure library logic and is covered by host tests.
                let reading = raw.into_reading();
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

        // Soil moisture changes slowly, so one sample per second is sufficient.
        soil_frame = soil_frame.wrapping_add(1);
        if soil_frame == 50 {
            soil_frame = 0;
            match nb::block!(adc.read_oneshot(&mut soil_pin)) {
                Ok(raw) => println!("SOIL,{},{}", telemetry_started.elapsed().as_millis(), raw),
                Err(_) => println!("Soil sensor ADC read failed"),
            }
        }

        delay.delay_millis(20);
    }
}

fn handle_startup_failure(error: StartupError) -> ! {
    let delay = Delay::new();

    // This is the firmware's safe state: no sensor-dependent work runs, but
    // diagnostics remain visible even if the serial monitor connects later.
    loop {
        match &error {
            StartupError::I2cConfiguration(cause) => {
                println!("I2C peripheral configuration failed: {:?}", cause);
            }
            StartupError::Sensor(cause) => {
                println!("MPU6050 initialization failed: {:?}", cause);
            }
            StartupError::LedConfiguration => {
                println!("Onboard RGB LED configuration failed");
            }
            StartupError::LedTransmission => {
                println!("Onboard RGB LED transmission failed");
            }
        }
        println!("Check wiring, then reset the ESP32 to retry initialization");
        delay.delay_millis(3_000);
    }
}
