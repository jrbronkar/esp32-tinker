#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    gpio::{Level, Pin},
    main,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Duration, Instant, Rate},
};
use esp_println::println;
use onboard_rgb::{interpolate, rgb_pulses, DEFAULT_BRIGHTNESS};

esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(all(feature = "devkitc-v1_0", feature = "devkitc-v1_1"))]
compile_error!("Select only one board revision feature");

#[cfg(not(any(feature = "devkitc-v1_0", feature = "devkitc-v1_1")))]
compile_error!("Select devkitc-v1_0 or devkitc-v1_1 to choose the RGB LED pin");

const FADE_STEPS: u16 = 64;
const FRAME_TIME: Duration = Duration::from_millis(20);

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
    let tx_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true);
    let mut channel = rmt
        .channel0
        .configure_tx(&tx_config)
        .unwrap()
        .with_pin(led_pin);

    let colors = [
        (0, DEFAULT_BRIGHTNESS, 0),
        (DEFAULT_BRIGHTNESS, 0, 0),
        (0, 0, DEFAULT_BRIGHTNESS),
    ];
    let mut current = (0, 0, 0);

    loop {
        println!("ESP32-S3 RGB gradient is still running!");

        for target in colors {
            for step in 1..=FADE_STEPS {
                let red = interpolate(current.0, target.0, step, FADE_STEPS);
                let green = interpolate(current.1, target.1, step, FADE_STEPS);
                let blue = interpolate(current.2, target.2, step, FADE_STEPS);
                let pulses = rgb_pulses(red, green, blue);
                channel = channel.transmit(&pulses).unwrap().wait().unwrap();
                wait(FRAME_TIME);
            }

            current = target;
        }
    }
}

fn wait(duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {}
}
