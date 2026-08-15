#![no_std]
#![no_main]

use core::convert::Infallible;

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    i2c::master::{Config, ConfigError, Error as I2cError, I2c},
    main,
    peripherals::Peripherals,
    time::Rate,
};
use esp_println::println;
use imu_demo::{Error as SensorError, Mpu6050, ADDRESS_LOW};

esp_bootloader_esp_idf::esp_app_desc!();

#[derive(Debug)]
enum StartupError {
    I2cConfiguration(ConfigError),
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
    // Run I²C at the MPU6050's conservative standard-mode frequency of 100 kHz.
    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let i2c = I2c::new(peripherals.I2C0, i2c_config)
        .map_err(StartupError::I2cConfiguration)?
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);

    // The breakout's disconnected AD0 pin selects the default 0x68 address.
    let mut sensor = Mpu6050::new(i2c, ADDRESS_LOW);
    let delay = Delay::new();

    println!("Initializing MPU6050 at I2C address 0x68...");

    sensor.initialize().map_err(StartupError::Sensor)?;

    println!("MPU6050 confirmed and awake");

    loop {
        // Read all seven sensor outputs in one I²C transaction so the axes
        // belong to the same measurement frame.
        match sensor.read_raw() {
            Ok(raw) => {
                println!(
                    "raw accel={:?} gyro={:?} temp={}",
                    raw.acceleration, raw.angular_velocity, raw.temperature,
                );

                // Conversion is pure library logic and is covered by host tests.
                let reading = raw.into_reading();
                println!(
                    "units accel(g) x={:.3} y={:.3} z={:.3} | gyro(dps) x={:.2} y={:.2} z={:.2} | temp={:.2} C",
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

        delay.delay_millis(500);
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
        }
        println!("Check wiring, then reset the ESP32 to retry initialization");
        delay.delay_millis(3_000);
    }
}
