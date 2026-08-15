extern crate alloc;

use core::{fmt::Write, net::Ipv4Addr};
use embassy_executor::Spawner;
use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Runner, StackResources,
};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_hal::{
    analog::adc::{Adc, AdcConfig, Attenuation},
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    ram,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    rng::Rng,
    time::{Instant, Rate},
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};
use heapless::String;
use onboard_rgb::{breathing_rgb, DEFAULT_BRIGHTNESS};
use soil_sensor::{Calibration, Smoother};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const SOIL_UDP_HOST: &str = env!("SOIL_UDP_HOST");
const SOIL_UDP_PORT: u16 = 7777;
const DRY_ADC: u16 = 4095;
const WATERED_ADC: u16 = 3425;

// Embassy tasks require their state to live for the entire firmware lifetime.
macro_rules! make_static {
    ($type:ty, $value:expr) => {{
        static CELL: static_cell::StaticCell<$type> = static_cell::StaticCell::new();
        CELL.uninit().write($value)
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // The radio stack allocates buffers dynamically. Reclaimed RAM comes from
    // boot-time memory that is no longer needed once the application starts.
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // esp-rtos supplies the timer and task scheduler used by Embassy and the
    // Wi-Fi driver. It runs alongside this application's async main task.
    let rtos_timer = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(rtos_timer.timer0, software_interrupt.software_interrupt0);
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap_or_else(|_| fatal("Wi-Fi interface creation failed"));
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());

    // DHCP lets the router assign the ESP32 an address. The runner task keeps
    // the IP stack moving while the connection task handles reconnects.
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        make_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );
    let connection_task =
        connection(controller).unwrap_or_else(|_| fatal("Wi-Fi connection task failed to start"));
    spawner.spawn(connection_task);
    let network_task =
        network_task(runner).unwrap_or_else(|_| fatal("Network task failed to start"));
    spawner.spawn(network_task);

    let destination_ip: Ipv4Addr = SOIL_UDP_HOST
        .parse()
        .unwrap_or_else(|_| fatal("SOIL_UDP_HOST must be an IPv4 address"));
    let mut rx_metadata = [PacketMetadata::EMPTY; 1];
    let mut rx_buffer = [0_u8; 64];
    let mut tx_metadata = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0_u8; 256];

    // UDP does not establish a persistent connection. Each reading is one
    // independent datagram, so a lost packet cannot stall later readings.
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_metadata,
        &mut rx_buffer,
        &mut tx_metadata,
        &mut tx_buffer,
    );
    socket
        .bind(0)
        .unwrap_or_else(|_| fatal("UDP socket failed to bind"));

    let mut adc_config = AdcConfig::new();
    let mut soil_pin = adc_config.enable_pin(peripherals.GPIO4, Attenuation::_11dB);
    let mut adc = Adc::new(peripherals.ADC1, adc_config);
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
    let mut smoother = Smoother::default();
    let started = Instant::now();
    let mut animation_frame = 0_u16;
    let mut sample_frame = 0_u8;
    let mut moisture_percent = 0_u8;

    println!("Waiting for Wi-Fi and DHCP...");
    stack.wait_config_up().await;
    println!("Wi-Fi ready: {:?}", stack.config_v4());
    println!("Sending soil telemetry to {destination_ip}:{SOIL_UDP_PORT}");
    loop {
        let red = u16::from(DEFAULT_BRIGHTNESS) * u16::from(100 - moisture_percent) / 100;
        let green = u16::from(DEFAULT_BRIGHTNESS) * u16::from(moisture_percent) / 100;
        let pulses = breathing_rgb(red as u8, green as u8, 0, animation_frame);
        led = led
            .transmit(&pulses)
            .and_then(|transfer| transfer.wait())
            .unwrap_or_else(|_| fatal("RGB LED transmission failed"));
        animation_frame = animation_frame.wrapping_add(1);

        // Animate every 20 ms but sample and transmit only once per second.
        sample_frame = sample_frame.wrapping_add(1);
        if sample_frame == 50 {
            sample_frame = 0;
            match nb::block!(adc.read_oneshot(&mut soil_pin)) {
                Ok(raw) => {
                    let filtered = smoother.update(raw);
                    moisture_percent = calibration.moisture_percent(filtered);
                    let mut record = String::<96>::new();
                    if write!(
                        record,
                        "SOIL,{},{},{},{}",
                        started.elapsed().as_millis(),
                        raw,
                        filtered,
                        moisture_percent
                    )
                    .is_ok()
                    {
                        println!("{record}");
                        if let Err(error) = socket
                            .send_to(record.as_bytes(), (destination_ip, SOIL_UDP_PORT))
                            .await
                        {
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

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        if let Err(error) = controller.connect_async().await {
            println!("Wi-Fi connection failed: {error:?}; retrying");
        } else {
            let disconnected = controller.wait_for_disconnect_async().await;
            println!("Wi-Fi disconnected: {disconnected:?}");
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn network_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

fn fatal(message: &str) -> ! {
    loop {
        println!("Soil firmware stopped safely: {message}");
        esp_hal::delay::Delay::new().delay_millis(3_000);
    }
}
