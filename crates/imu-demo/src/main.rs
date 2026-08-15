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

esp_bootloader_esp_idf::esp_app_desc!();

const MPU6050_ADDRESS_LOW: u8 = 0x68;
const MPU6050_ADDRESS_HIGH: u8 = 0x69;
const WHO_AM_I_REGISTER: u8 = 0x75;

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let i2c_config = Config::default().with_frequency(Rate::from_khz(100));
    let mut i2c = I2c::new(peripherals.I2C0, i2c_config)
        .unwrap()
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9);
    let delay = Delay::new();

    println!("ESP32-S3 MPU6050 I2C scanner started");
    println!("Wiring: GPIO8=SDA, GPIO9=SCL");

    loop {
        let mut devices_found = 0;

        println!("Scanning I2C addresses...");

        // The ranges below skip addresses reserved by the I2C specification.
        for address in 0x08..=0x77 {
            if i2c.write(address, &[]).is_ok() {
                devices_found += 1;
                println!("Found I2C device at 0x{address:02X}");

                if address == MPU6050_ADDRESS_LOW || address == MPU6050_ADDRESS_HIGH {
                    report_mpu6050_identity(&mut i2c, address);
                }
            }
        }

        if devices_found == 0 {
            println!("No I2C devices found; check power, ground, SDA, and SCL");
        }

        println!("Scan complete; {devices_found} device(s) found\n");
        delay.delay_millis(3_000);
    }
}

fn report_mpu6050_identity(i2c: &mut I2c<'_, esp_hal::Blocking>, address: u8) {
    let mut identity = [0_u8; 1];

    match i2c.write_read(address, &[WHO_AM_I_REGISTER], &mut identity) {
        Ok(()) if identity[0] == MPU6050_ADDRESS_LOW => {
            println!("MPU6050 confirmed: WHO_AM_I = 0x{:02X}", identity[0]);
        }
        Ok(()) => {
            println!("Device responded, but WHO_AM_I = 0x{:02X}", identity[0]);
        }
        Err(_) => {
            println!("Device acknowledged, but reading WHO_AM_I failed");
        }
    }
}
