#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    analog::adc::{Adc, AdcConfig, Attenuation},
    delay::Delay,
    main,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use onboard_rgb::breathing_green;

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // GPIO4 reads the probe's analog output through ADC1.
    let mut adc_config = AdcConfig::new();
    let mut soil_pin = adc_config.enable_pin(peripherals.GPIO4, Attenuation::_11dB);
    let mut adc = Adc::new(peripherals.ADC1, adc_config);

    // A green breathing LED shows that the sampling loop is alive.
    let rmt = match Rmt::new(peripherals.RMT, Rate::from_mhz(80)) {
        Ok(rmt) => rmt,
        Err(_) => fatal("RGB LED clock configuration failed"),
    };
    let led_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(esp_hal::gpio::Level::Low)
        .with_idle_output(true);
    let mut led = match rmt.channel0.configure_tx(&led_config) {
        Ok(channel) => channel.with_pin(peripherals.GPIO48),
        Err(_) => fatal("RGB LED channel configuration failed"),
    };

    let delay = Delay::new();
    let started = Instant::now();
    let mut animation_frame = 0_u16;
    let mut sample_frame = 0_u8;

    println!("Capacitive soil-moisture demo started");
    println!("SOIL_CSV,time_ms,raw_adc");

    loop {
        let pulses = breathing_green(animation_frame);
        led = match led.transmit(&pulses) {
            Ok(transfer) => match transfer.wait() {
                Ok(channel) => channel,
                Err(_) => fatal("RGB LED transmission failed"),
            },
            Err(_) => fatal("RGB LED transmission failed"),
        };
        animation_frame = animation_frame.wrapping_add(1);

        // Soil moisture changes slowly, so one ADC reading per second is enough.
        sample_frame = sample_frame.wrapping_add(1);
        if sample_frame == 50 {
            sample_frame = 0;
            match nb::block!(adc.read_oneshot(&mut soil_pin)) {
                Ok(raw) => println!("SOIL,{},{}", started.elapsed().as_millis(), raw),
                Err(_) => println!("Soil sensor ADC read failed"),
            }
        }

        delay.delay_millis(20);
    }
}

fn fatal(message: &str) -> ! {
    let delay = Delay::new();
    loop {
        println!("Soil firmware stopped safely: {message}");
        delay.delay_millis(3_000);
    }
}
