use esp_hal::{
    delay::Delay,
    main,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use onboard_rgb::{breathing_rgb, DEFAULT_BRIGHTNESS};
use soil_sensor::{esp32s3::SoilProbe, Calibration};

// These provisional endpoints came from this probe. Replace DRY_ADC with the
// stable value observed at the soil dryness where watering should begin.
const DRY_ADC: u16 = 4095;
const WATERED_ADC: u16 = 3425;

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80))
        .unwrap_or_else(|_| fatal("RGB LED clock configuration failed"));
    let led_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(esp_hal::gpio::Level::Low)
        .with_idle_output(true);
    let mut led = rmt
        .channel0
        .configure_tx(&led_config)
        .map(|channel| channel.with_pin(peripherals.GPIO48))
        .unwrap_or_else(|_| fatal("RGB LED channel configuration failed"));
    let delay = Delay::new();
    let calibration = Calibration::new(DRY_ADC, WATERED_ADC)
        .unwrap_or_else(|_| fatal("Soil calibration endpoints are invalid"));
    let mut soil = SoilProbe::new(peripherals.ADC1, peripherals.GPIO4, calibration);
    let started = Instant::now();
    let mut animation_frame = 0_u16;
    let mut sample_frame = 0_u8;
    let mut moisture_percent = 0_u8;

    println!("Capacitive soil-moisture demo started (serial mode)");
    println!("SOIL_CSV,time_ms,raw_adc,filtered_adc,moisture_percent");
    println!("Calibration is provisional: dry={DRY_ADC}, watered={WATERED_ADC}");
    loop {
        // Green means wet, yellow is midway, and red means dry.
        let red = u16::from(DEFAULT_BRIGHTNESS) * u16::from(100 - moisture_percent) / 100;
        let green = u16::from(DEFAULT_BRIGHTNESS) * u16::from(moisture_percent) / 100;
        let pulses = breathing_rgb(red as u8, green as u8, 0, animation_frame);
        led = led
            .transmit(&pulses)
            .and_then(|transfer| transfer.wait())
            .unwrap_or_else(|_| fatal("RGB LED transmission failed"));
        animation_frame = animation_frame.wrapping_add(1);
        // The LED animates every 20 ms; the slow-changing soil is sampled once
        // per second to keep the serial output readable.
        sample_frame = sample_frame.wrapping_add(1);
        if sample_frame == 50 {
            sample_frame = 0;
            match soil.read() {
                Ok(reading) => {
                    moisture_percent = reading.moisture_percent;
                    println!(
                        "SOIL,{},{},{},{}",
                        started.elapsed().as_millis(),
                        reading.raw_adc,
                        reading.filtered_adc,
                        moisture_percent
                    );
                }
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
