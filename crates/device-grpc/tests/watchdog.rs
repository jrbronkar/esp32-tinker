use core::time::Duration;
use device_grpc::watchdog::UploadWatchdog;
use tokio::time::Instant;
#[tokio::test(start_paused = true)]
async fn inactive_upload_expires_at_five_seconds() {
    let timer = Instant::now();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.elapsed());
    assert!(
        tokio::time::timeout(
            Duration::from_secs(4),
            watchdog.expired(
                || timer.elapsed(),
                |deadline| tokio::time::sleep_until(timer + deadline)
            )
        )
        .await
        .is_err()
    );
    watchdog
        .expired(
            || timer.elapsed(),
            |deadline| tokio::time::sleep_until(timer + deadline),
        )
        .await;
    assert_eq!(timer.elapsed(), Duration::from_secs(5));
}
#[tokio::test(start_paused = true)]
async fn activity_replaces_old_deadline_without_premature_expiry() {
    let timer = Instant::now();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.elapsed());
    tokio::select! {
        _=watchdog.expired(|| timer.elapsed(), |deadline| tokio::time::sleep_until(timer + deadline))=>panic!("expired despite upload activity"),
        _=async{tokio::time::sleep(Duration::from_secs(3)).await;watchdog.activity(timer.elapsed());tokio::time::sleep(Duration::from_secs(4)).await;}=>{},
    }
    watchdog
        .expired(
            || timer.elapsed(),
            |deadline| tokio::time::sleep_until(timer + deadline),
        )
        .await;
    assert_eq!(timer.elapsed(), Duration::from_secs(8));
}
#[tokio::test(start_paused = true)]
async fn released_or_committing_upload_cancels_deadline() {
    let timer = Instant::now();
    let watchdog = UploadWatchdog::new();
    watchdog.activity(timer.elapsed());
    tokio::select! {
        _=watchdog.expired(|| timer.elapsed(), |deadline| tokio::time::sleep_until(timer + deadline))=>panic!("stale deadline after release"),
        _=async{tokio::time::sleep(Duration::from_secs(3)).await;watchdog.clear();tokio::time::sleep(Duration::from_secs(60)).await;}=>{},
    }
    assert_eq!(timer.elapsed(), Duration::from_secs(63));
}
