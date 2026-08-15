#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    gpio::{Level, Pin},
    main,
    rmt::{PulseCode, Rmt, TxChannelConfig, TxChannelCreator},
    time::{Duration, Instant, Rate},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(all(feature = "devkitc-v1_0", feature = "devkitc-v1_1"))]
compile_error!("Select only one board revision feature");

#[cfg(not(any(feature = "devkitc-v1_0", feature = "devkitc-v1_1")))]
compile_error!("Select devkitc-v1_0 or devkitc-v1_1 to choose the RGB LED pin");

const LED_BRIGHTNESS: u8 = 16;

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // The original DevKitC-1 uses GPIO48; revision v1.1 uses GPIO38.
    #[cfg(feature = "devkitc-v1_0")]
    let led_pin = peripherals.GPIO48.degrade();
    #[cfg(feature = "devkitc-v1_1")]
    let led_pin = peripherals.GPIO38.degrade();

    // RMT generates the precisely timed pulses required by the addressable LED.
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let mut channel = rmt
        .channel0
        .configure_tx(&TxChannelConfig::default())
        .unwrap()
        .with_pin(led_pin);

    loop {
        println!("Hello from our green ESP32-S3!");
        let pulses = rgb_pulses(0, LED_BRIGHTNESS, 0);
        channel = channel.transmit(&pulses).unwrap().wait().unwrap();
        wait_one_second();

        println!("Hello from our red ESP32-S3!");
        let pulses = rgb_pulses(LED_BRIGHTNESS, 0, 0);
        channel = channel.transmit(&pulses).unwrap().wait().unwrap();
        wait_one_second();

        println!("Hello from our blue ESP32-S3!");
        let pulses = rgb_pulses(0, 0, LED_BRIGHTNESS);
        channel = channel.transmit(&pulses).unwrap().wait().unwrap();
        wait_one_second();
    }
}

fn rgb_pulses(red: u8, green: u8, blue: u8) -> [PulseCode; 25] {
    // Addressable LEDs expect color bytes in green-red-blue order.
    let colors = [green, red, blue];
    let mut pulses = [PulseCode::end_marker(); 25];
    let mut pulse_index = 0;

    for color in colors {
        for bit in (0..8).rev() {
            let is_one = color & (1 << bit) != 0;
            pulses[pulse_index] = if is_one {
                PulseCode::new(Level::High, 64, Level::Low, 36)
            } else {
                PulseCode::new(Level::High, 32, Level::Low, 68)
            };
            pulse_index += 1;
        }
    }

    pulses
}

fn wait_one_second() {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {}
}
