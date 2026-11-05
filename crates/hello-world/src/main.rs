// This is a bare-metal program, so Rust's normal standard library is unavailable.
#![no_std]
// The ESP32 reset handler supplies our entry point instead of a desktop-style `main`.
#![no_main]

// Install a panic handler that prints useful crash information over the serial connection.
use esp_backtrace as _;

// Import the ESP32 entry-point macro and the time types used for the one-second delay.
use esp_hal::{
    main,
    time::{Duration, Instant},
};

// Import a `println!` implementation that writes through the ESP32's serial interface.
use esp_println::println;

// Embed the metadata structure expected by the ESP-IDF bootloader and `espflash`.
// It includes fields such as this crate's name and version from Cargo.toml.
esp_bootloader_esp_idf::esp_app_desc!();

// Mark this function as the firmware entry point called after the chip resets.
#[main]
fn main() -> ! {
    // Initialize the chip's clocks and peripherals using esp-hal's default configuration.
    // Keeping the returned peripheral handles alive also preserves exclusive ownership of them.
    let _peripherals = esp_hal::init(esp_hal::Config::default());

    // Keep sending a heartbeat so the serial monitor shows that the ESP32 is running.
    loop {
        println!("Hello from the ESP32-S3!");

        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {}
    }
}
