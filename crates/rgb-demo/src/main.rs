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
        (0, LED_BRIGHTNESS, 0),
        (LED_BRIGHTNESS, 0, 0),
        (0, 0, LED_BRIGHTNESS),
    ];
    let mut current = (0, 0, 0);

    println!("Hello from our ESP32-S3 RGB gradient!");

    loop {
        for target in colors {
            for step in 1..=FADE_STEPS {
                let red = interpolate(current.0, target.0, step);
                let green = interpolate(current.1, target.1, step);
                let blue = interpolate(current.2, target.2, step);
                let pulses = rgb_pulses(red, green, blue);
                channel = channel.transmit(&pulses).unwrap().wait().unwrap();
                wait(FRAME_TIME);
            }

            current = target;
        }
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

fn interpolate(start: u8, end: u8, step: u16) -> u8 {
    let start = i32::from(start);
    let change = i32::from(end) - start;
    (start + change * i32::from(step) / i32::from(FADE_STEPS)) as u8
}

fn wait(duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {}
}
