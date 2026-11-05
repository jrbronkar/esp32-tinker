use core::time::Duration;
use device_grpc::watchdog::UploadWatchdog;
use protolink::{Clock, tokio::TokioTimer};
#[tokio::test(start_paused = true)]
async fn inactive_upload_expires_at_five_seconds() {
    let timer = TokioTimer::new();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.now());
    assert!(
        tokio::time::timeout(Duration::from_secs(4), watchdog.expired(&timer))
            .await
            .is_err()
    );
    watchdog.expired(&timer).await;
    assert_eq!(timer.now(), Duration::from_secs(5));
}
#[tokio::test(start_paused = true)]
async fn activity_replaces_old_deadline_without_premature_expiry() {
    let timer = TokioTimer::new();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.now());
    tokio::select! {
        _=watchdog.expired(&timer)=>panic!("expired despite upload activity"),
        _=async{tokio::time::sleep(Duration::from_secs(3)).await;watchdog.activity(timer.now());tokio::time::sleep(Duration::from_secs(4)).await;}=>{},
    }
    watchdog.expired(&timer).await;
    assert_eq!(timer.now(), Duration::from_secs(8));
}
#[tokio::test(start_paused = true)]
async fn released_or_committing_upload_cancels_deadline() {
    let timer = TokioTimer::new();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.now());
    tokio::select! {
        _=watchdog.expired(&timer)=>panic!("stale deadline after release"),
        _=async{tokio::time::sleep(Duration::from_secs(3)).await;watchdog.clear();tokio::time::sleep(Duration::from_secs(60)).await;}=>{},
    }
    assert_eq!(timer.now(), Duration::from_secs(63));
}
