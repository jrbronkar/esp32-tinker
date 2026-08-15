#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    i2c::master::{Config, I2c},
    main,
    time::Rate,
};
use esp_println::println;
use imu_demo::{Mpu6050, ADDRESS_LOW};

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    // Take ownership of the chip peripherals, then run I²C at the MPU6050's
    // conservative standard-mode frequency of 100 kHz.
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let i2c = I2c::new(peripherals.I2C0, i2c_config)
        .unwrap()
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);

    // The breakout's disconnected AD0 pin selects the default 0x68 address.
    let mut sensor = Mpu6050::new(i2c, ADDRESS_LOW);
    let delay = Delay::new();

    println!("Initializing MPU6050 at I2C address 0x68...");

    if let Err(error) = sensor.initialize() {
        println!("MPU6050 initialization failed: {:?}", error);
        loop {
            // Stay alive and keep reporting the fault so a newly attached
            // serial monitor does not miss a one-time initialization message.
            delay.delay_millis(3_000);
            println!("MPU6050 is not running; reset after checking the wiring");
        }
    }

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
