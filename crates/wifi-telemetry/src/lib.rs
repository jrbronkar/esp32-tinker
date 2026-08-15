#![no_std]

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Runner, StackResources,
};
use embassy_time::{Duration, Timer};
use esp_hal::{
    interrupt::software::SoftwareInterruptControl,
    peripherals::{SW_INTERRUPT, TIMG0, WIFI},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};

const MAX_RECORD_BYTES: usize = 192;

/// The network settings embedded into a Wi-Fi-enabled firmware build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub ssid: &'static str,
    pub password: &'static str,
    pub destination: Ipv4Addr,
    pub port: u16,
}

/// A connected UDP sender that adds one newline to every telemetry record.
pub struct UdpTelemetry {
    socket: UdpSocket<'static>,
    destination: (Ipv4Addr, u16),
}

/// Initialization failures are intentionally credential-free and safe to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    InvalidSettings,
    Radio,
    ConnectionTask,
    NetworkTask,
    Socket,
}

/// Sending can fail because the network is temporarily unavailable or because
/// a caller supplied a record larger than the fixed no-allocation buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    RecordTooLong,
    Network,
}

macro_rules! make_static {
    ($type:ty, $value:expr) => {{
        static CELL: static_cell::StaticCell<$type> = static_cell::StaticCell::new();
        CELL.uninit().write($value)
    }};
}

/// Starts the radio, reconnect task, DHCP stack, and UDP socket.
///
/// `esp_alloc` must be initialized by the firmware before calling this function.
pub async fn connect(
    spawner: Spawner,
    wifi: WIFI<'static>,
    timer: TIMG0<'static>,
    software_interrupt: SW_INTERRUPT<'static>,
    settings: Settings,
) -> Result<UdpTelemetry, InitError> {
    if settings.ssid.is_empty() || settings.port == 0 {
        return Err(InitError::InvalidSettings);
    }

    let rtos_timer = TimerGroup::new(timer);
    let software_interrupt = SoftwareInterruptControl::new(software_interrupt);
    esp_rtos::start(rtos_timer.timer0, software_interrupt.software_interrupt0);

    let station = Config::Station(
        StationConfig::default()
            .with_ssid(settings.ssid)
            .with_password(settings.password.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        wifi,
        ControllerConfig::default().with_initial_config(station),
    )
    .map_err(|_| InitError::Radio)?;

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        make_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    let connection_task = connection(controller).map_err(|_| InitError::ConnectionTask)?;
    spawner.spawn(connection_task);
    let network_task = network_task(runner).map_err(|_| InitError::NetworkTask)?;
    spawner.spawn(network_task);

    let rx_metadata = make_static!([PacketMetadata; 1], [PacketMetadata::EMPTY; 1]);
    let rx_buffer = make_static!([u8; 64], [0; 64]);
    let tx_metadata = make_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
    let tx_buffer = make_static!([u8; 256], [0; 256]);
    let mut socket = UdpSocket::new(stack, rx_metadata, rx_buffer, tx_metadata, tx_buffer);
    socket.bind(0).map_err(|_| InitError::Socket)?;

    println!("Waiting for Wi-Fi and DHCP...");
    stack.wait_config_up().await;
    println!("Wi-Fi ready: {:?}", stack.config_v4());
    println!(
        "Sending telemetry to {}:{}",
        settings.destination, settings.port
    );

    Ok(UdpTelemetry {
        socket,
        destination: (settings.destination, settings.port),
    })
}

impl UdpTelemetry {
    /// Sends one record as one UDP datagram, appending a newline for simple
    /// receivers such as `nc -u -l 7777`.
    pub async fn send_line(&mut self, record: &str) -> Result<(), SendError> {
        if record.len() + 1 > MAX_RECORD_BYTES {
            return Err(SendError::RecordTooLong);
        }

        let mut packet = [0_u8; MAX_RECORD_BYTES];
        packet[..record.len()].copy_from_slice(record.as_bytes());
        packet[record.len()] = b'\n';
        self.socket
            .send_to(&packet[..record.len() + 1], self.destination)
            .await
            .map_err(|_| SendError::Network)
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
