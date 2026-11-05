//! Initialize shared management state, gRPC and DNS-SD.
use embassy_net::Stack;
use esp_println::println;
use ota_protocol::StatusResponse;
#[embassy_executor::task]
pub async fn server(
    stack: Stack<'static>,
    identity: StatusResponse,
    flash: esp_storage::FlashStorage<'static>,
    logs: &'static crate::DeviceLogs,
) {
    let mut identity = identity;
    let mut target = crate::flash_target::FlashTarget::new(flash);
    identity.ota_available = target.available;

    let runtime = crate::runtime::Runtime::new(logs);
    if let Err(error) = target.confirm_running() {
        runtime.log(
            ota_protocol::LogSeverity::Error,
            "Running image confirmation failed",
        );
        println!("Running image confirmation failed: {error:?}");
    }
    runtime.log(ota_protocol::LogSeverity::Info, "Management service ready");
    // Both futures borrow task-owned state. Joining keeps them concurrent without
    // requiring a global runtime or a separate spawned task with a static borrow.
    let target_offset = target.offset;
    let target = core::cell::RefCell::new(target);
    embassy_futures::join::join(
        crate::grpc::serve(stack, &identity, &runtime, &target, target_offset),
        crate::discovery::serve(stack, &identity, &runtime),
    )
    .await;
}
