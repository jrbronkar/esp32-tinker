#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{clock::CpuClock, ram};
use esp_println::println;

#[cfg(not(feature = "dev-ota"))]
esp_bootloader_esp_idf::esp_app_desc!();
#[cfg(feature = "dev-ota")]
esp_bootloader_esp_idf::esp_app_desc!(
    match option_env!("FIRMWARE_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    },
    "device-demo",
    esp_bootloader_esp_idf::BUILD_TIME,
    esp_bootloader_esp_idf::BUILD_DATE,
    "rust-dev-ota-v2",
    esp_bootloader_esp_idf::MMU_PAGE_SIZE,
    0,
    u16::MAX,
    0
);

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let (boot, flash) = device_service::boot::inspect(peripherals.FLASH);
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let stack = wifi_network::connect(
        spawner,
        peripherals.WIFI,
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        wifi_network::Settings {
            ssid: env!("WIFI_SSID"),
            password: env!("WIFI_PASSWORD"),
        },
    )
    .await
    .expect("Wi-Fi initialization failed");
    device_service::start_status(
        spawner,
        stack,
        "device-demo",
        option_env!("FIRMWARE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        boot,
        flash,
    )
    .expect("Status service initialization failed");
    #[cfg(not(feature = "dev-ota"))]
    println!("device-demo ready: status only; no sensors, telemetry, or flash writes");
    #[cfg(feature = "dev-ota")]
    println!("device-demo ready: UNSIGNED DEVELOPMENT OTA; requires provisioned layout");
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!("device-demo alive");
    }
}
