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

    let stack = match wifi_network::connect(
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
    {
        Ok(stack) => stack,
        Err(error) => {
            println!("Wi-Fi initialization failed: {error:?}");
            restart_initialization().await
        }
    };
    if let Err(error) = device_service::start_status(
        spawner,
        stack,
        "device-demo",
        option_env!("FIRMWARE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        boot,
        flash,
    ) {
        println!("Device service initialization failed: {error}");
        restart_initialization().await;
    }
    #[cfg(not(feature = "dev-ota"))]
    println!("device-demo ready: status only; no sensors, telemetry, or flash writes");
    #[cfg(feature = "dev-ota")]
    println!("device-demo ready: UNSIGNED DEVELOPMENT OTA; requires provisioned layout");
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!("device-demo alive");
    }
}

/// Startup consumes peripheral handles and initializes static resources once.
/// Retry from reset rather than reinitializing them in the same execution.
async fn restart_initialization() -> ! {
    println!("Retrying device initialization through reset in five seconds");
    Timer::after(Duration::from_secs(5)).await;
    esp_hal::system::software_reset();
}
