#![no_std]

use embassy_executor::Spawner;
pub mod boot;
mod discovery;
mod flash_target;
mod grpc;
mod mdns_filter;
mod runtime;
mod status;

pub use ota_protocol::LogSeverity;
pub use runtime::DeviceLogs;

/// Start status and development OTA on an existing network stack.
pub fn start_status(
    spawner: Spawner,
    stack: embassy_net::Stack<'static>,
    application: &str,
    firmware_version: &str,
    boot: ota_protocol::BootInfo,
    flash: esp_storage::FlashStorage<'static>,
    logs: &'static DeviceLogs,
) -> Result<(), &'static str> {
    use core::fmt::Write;
    let mac = esp_hal::efuse::base_mac_address();
    let mut id = heapless::String::<64>::new();
    write!(id, "esp32s3-{mac}").map_err(|_| "device identity too long")?;
    let mut identity = ota_protocol::status(&id, application, firmware_version, 0)
        .map_err(|_| "status identity exceeds protocol bounds")?;
    identity.set_boot(boot);
    let task =
        status::server(stack, identity, flash, logs).map_err(|_| "status task unavailable")?;
    spawner.spawn(task);
    Ok(())
}
