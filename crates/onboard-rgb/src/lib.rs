#![no_std]
#![deny(missing_docs)]

//! Shared color animation and pulse encoding for the DevKitC-1 onboard LED.

use esp_hal::{gpio::Level, rmt::PulseCode};

/// Low maximum channel value that keeps the small onboard LED comfortable.
pub const DEFAULT_BRIGHTNESS: u8 = 16;

/// Number of animation frames in one complete breathing cycle.
pub const BREATHING_FRAMES: u16 = 128;

/// Encodes one RGB color as the 24 timed pulses expected by the onboard LED.
///
/// SK68/WS2812-style LEDs receive bytes in green-red-blue order. The final
/// marker ends the RMT transfer; the delay before the next frame latches it.
pub fn rgb_pulses(red: u8, green: u8, blue: u8) -> [PulseCode; 25] {
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

/// Linearly interpolates one color channel across a fixed number of steps.
pub fn interpolate(start: u8, end: u8, step: u16, total_steps: u16) -> u8 {
    let start = i32::from(start);
    let change = i32::from(end) - start;
    (start + change * i32::from(step) / i32::from(total_steps)) as u8
}

/// Produces a dim green breathing frame for a monotonically increasing index.
pub fn breathing_green(frame: u16) -> [PulseCode; 25] {
    breathing_rgb(0, DEFAULT_BRIGHTNESS, 0, frame)
}

/// Produces a breathing frame whose peak is the supplied RGB color.
pub fn breathing_rgb(red: u8, green: u8, blue: u8, frame: u16) -> [PulseCode; 25] {
    let phase = frame % BREATHING_FRAMES;
    let half = BREATHING_FRAMES / 2;
    let ramp = if phase < half {
        phase
    } else {
        BREATHING_FRAMES - phase
    };
    let scale = |channel: u8| (ramp * u16::from(channel) / half) as u8;
    rgb_pulses(scale(red), scale(green), scale(blue))
}
