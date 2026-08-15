#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    main,
    time::{Duration, Instant},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());

    loop {
        println!("Hello, ESP32-S3!");

        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {}
    }
}
