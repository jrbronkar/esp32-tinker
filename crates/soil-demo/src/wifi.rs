extern crate alloc;

use core::{fmt::Write, net::Ipv4Addr};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_hal::{
    clock::CpuClock,
    ram,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::{Instant, Rate},
};
use esp_println::println;
use heapless::String;
use onboard_rgb::{breathing_rgb, DEFAULT_BRIGHTNESS};
use soil_sensor::{esp32s3::SoilProbe, Calibration};
use wifi_network::Settings;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const SOIL_UDP_HOST: &str = env!("SOIL_UDP_HOST");
const SOIL_UDP_PORT: u16 = 7777;
const DRY_ADC: u16 = 4095;
const WATERED_ADC: u16 = 3425;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // The radio stack dynamically allocates its internal packet buffers.
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let destination: Ipv4Addr = SOIL_UDP_HOST
        .parse()
        .unwrap_or_else(|_| fatal("SOIL_UDP_HOST must be an IPv4 address"));
    let stack = wifi_network::connect(
        spawner,
        peripherals.WIFI,
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        Settings {
            ssid: WIFI_SSID,
            password: WIFI_PASSWORD,
        },
    )
    .await
    .unwrap_or_else(|_| fatal("Wi-Fi telemetry initialization failed"));

    let mut telemetry = wifi_telemetry::UdpTelemetry::new(stack, destination, SOIL_UDP_PORT)
        .unwrap_or_else(|_| fatal("UDP telemetry initialization failed"));

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
    let calibration = Calibration::new(DRY_ADC, WATERED_ADC)
        .unwrap_or_else(|_| fatal("Soil calibration endpoints are invalid"));
    let mut soil = SoilProbe::new(peripherals.ADC1, peripherals.GPIO4, calibration);
    let started = Instant::now();
    let mut animation_frame = 0_u16;
    let mut sample_frame = 0_u8;
    let mut moisture_percent = 0_u8;

    loop {
        let red = u16::from(DEFAULT_BRIGHTNESS) * u16::from(100 - moisture_percent) / 100;
        let green = u16::from(DEFAULT_BRIGHTNESS) * u16::from(moisture_percent) / 100;
        let pulses = breathing_rgb(red as u8, green as u8, 0, animation_frame);
        led = led
            .transmit(&pulses)
            .and_then(|transfer| transfer.wait())
            .unwrap_or_else(|_| fatal("RGB LED transmission failed"));
        animation_frame = animation_frame.wrapping_add(1);

        sample_frame = sample_frame.wrapping_add(1);
        if sample_frame == 50 {
            sample_frame = 0;
            match soil.read() {
                Ok(reading) => {
                    moisture_percent = reading.moisture_percent;
                    let mut record = String::<96>::new();
                    if write!(
                        record,
                        "SOIL,{},{},{},{}",
                        started.elapsed().as_millis(),
                        reading.raw_adc,
                        reading.filtered_adc,
                        moisture_percent
                    )
                    .is_ok()
                    {
                        println!("{record}");
                        if let Err(error) = telemetry.send_line(&record).await {
                            println!("UDP send failed: {error:?}");
                        }
                    }
                }
                Err(_) => println!("Soil sensor ADC read failed"),
            }
        }
        Timer::after(Duration::from_millis(20)).await;
    }
}

fn fatal(message: &str) -> ! {
    loop {
        println!("Soil firmware stopped safely: {message}");
        esp_hal::delay::Delay::new().delay_millis(3_000);
    }
}
