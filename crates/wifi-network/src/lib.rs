#![no_std]

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::{
    interrupt::software::SoftwareInterruptControl,
    peripherals::{SW_INTERRUPT, TIMG0, WIFI},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};

/// Credentials embedded in the firmware by the application.
#[derive(Clone, Copy)]
pub struct Settings {
    pub ssid: &'static str,
    pub password: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    InvalidSettings,
    Radio,
    ConnectionTask,
    NetworkTask,
}

macro_rules! make_static {
    ($type:ty, $value:expr) => {{
        static CELL: static_cell::StaticCell<$type> = static_cell::StaticCell::new();
        CELL.uninit().write($value)
    }};
}

/// Starts RTOS, radio, reconnection, and DHCP once. Initialize esp_alloc first.
pub async fn connect(
    spawner: Spawner,
    wifi: WIFI<'static>,
    timer: TIMG0<'static>,
    software_interrupt: SW_INTERRUPT<'static>,
    settings: Settings,
) -> Result<Stack<'static>, InitError> {
    if settings.ssid.is_empty() {
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
        make_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );

    let connection_task = connection(controller).map_err(|_| InitError::ConnectionTask)?;
    spawner.spawn(connection_task);
    let network_task = network_task(runner).map_err(|_| InitError::NetworkTask)?;
    spawner.spawn(network_task);

    println!("Waiting for Wi-Fi and DHCP...");
    stack.wait_config_up().await;
    println!("Wi-Fi ready: {:?}", stack.config_v4());
    Ok(stack)
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
