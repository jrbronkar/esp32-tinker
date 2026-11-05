use ota_protocol::{UploadOperation as Op, UploadPhase};
use std::time::Duration;

#[path = "support/grpc.rs"]
mod common;
#[path = "support/ota.rs"]
mod ota;
use common::*;
use ota::image;

async fn fixture(
    fault: &'static str,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    ota::State,
) {
    ota::start(fault, false).await
}

async fn client_upload(
    address: std::net::SocketAddr,
    commit: bool,
    tag: &str,
) -> std::io::Result<espctl::upload::UploadResult> {
    let path = std::env::temp_dir().join(format!("espctl-grpc-{}-{tag}.bin", std::process::id()));
    std::fs::write(&path, image()).unwrap();
    tokio::task::spawn_blocking(move || {
        let result = espctl::upload::upload(address, Duration::from_secs(2), &path, "v1", commit);
        std::fs::remove_file(path).unwrap();
        result
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn upload_verifies_and_activates_only_when_explicitly_requested() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for commit in [false, true] {
                let (address, server, state) = fixture("").await;
                let result =
                    client_upload(address, commit, if commit { "commit" } else { "verify" })
                        .await
                        .unwrap();
                assert_eq!(result.committed, commit);
                assert_eq!(result.bytes, image().len() as u32);
                assert_eq!(result.target_offset, 0x310000);
                assert_eq!(state.commits.get(), usize::from(commit));
                assert_eq!(state.pending.get(), commit);
                assert_eq!(
                    *state.requests.borrow(),
                    vec![
                        "begin",
                        "chunk",
                        "finish",
                        if commit { "commit" } else { "abort" }
                    ]
                );
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn upload_rejects_bad_acknowledgements_and_does_not_replay() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["id", "phase", "offset", "target", "truncated", "disconnect"] {
                let (address, server, state) = fixture(fault).await;
                assert!(
                    client_upload(address, true, fault).await.is_err(),
                    "{fault}"
                );
                assert_eq!(*state.requests.borrow(), vec!["begin"]);
                assert_eq!(state.commits.get(), 0);
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn lost_commit_reply_reports_uncertainty_without_replay() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["lost-commit", "commit-storage", "disconnect-after-commit"] {
                let (address, server, state) = fixture(fault).await;
                let error = client_upload(address, true, fault).await.unwrap_err();
                assert!(
                    error.to_string().contains("commit outcome uncertain"),
                    "{error}"
                );
                assert_eq!(state.commits.get(), 1);
                assert!(state.pending.get());
                assert_eq!(
                    *state.requests.borrow(),
                    vec!["begin", "chunk", "finish", "commit"]
                );
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn commit_before_verification_cannot_change_boot_selection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("").await;
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut client = Client::new(channel);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            assert_eq!(
                op(&tx, &mut replies, Op::Commit(Default::default()), 2)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            assert_eq!(state.commits.get(), 0);
            assert!(!state.pending.get());
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn commit_latch_prevents_any_further_upload_until_reboot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("").await;
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut client = Client::new(channel);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            op(&tx, &mut replies, chunk(0, &image()), 2).await.unwrap();
            op(&tx, &mut replies, Op::Finish(Default::default()), 3)
                .await
                .unwrap();
            let ack = op(&tx, &mut replies, Op::Commit(Default::default()), 4)
                .await
                .unwrap();
            assert_eq!(ack.phase, UploadPhase::Committed);
            assert!(replies.message().await.unwrap().is_none());
            drop(tx);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            assert_eq!(
                op(&tx, &mut replies, begin(&image(), "v1"), 1)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            assert_eq!(state.commits.get(), 1);
            assert_eq!(state.writes.get(), 1);
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn cli_upload_logs_share_connection_and_log_errors_preserve_commit_outcome() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["", "bad-logs", "lost-commit"] {
                let (address, server, state) = fixture(fault).await;
                let path = std::env::temp_dir().join(format!(
                    "espctl-log-upload-{}-{fault}.bin",
                    std::process::id()
                ));
                std::fs::write(&path, image()).unwrap();
                let (result, events) = tokio::task::spawn_blocking(move || {
                    let mut events = Vec::new();
                    let result = espctl::upload::upload_with_logs(
                        address,
                        Duration::from_secs(2),
                        &path,
                        "v1",
                        true,
                        &mut |batch| events.push(batch),
                    );
                    std::fs::remove_file(path).unwrap();
                    (result, events)
                })
                .await
                .unwrap();
                if fault == "lost-commit" {
                    assert!(result
                        .unwrap_err()
                        .to_string()
                        .contains("commit outcome uncertain"));
                } else {
                    assert!(result.unwrap().committed);
                }
                if fault == "bad-logs" {
                    assert!(events.iter().any(|e| e.is_err()));
                } else {
                    assert!(events
                        .iter()
                        .filter_map(|e| e.as_ref().ok())
                        .flat_map(|b| b.entries.iter())
                        .any(|e| e.message.starts_with("gRPC chunk")));
                }
                assert_eq!(state.commits.get(), 1);
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[test]
fn cli_exposes_grpc_upload_logs_and_rejects_removed_legacy_option() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args(["upload", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in ["--logs", "7779"] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("--legacy"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args([
            "upload",
            "--host",
            "127.0.0.1",
            "--image",
            "missing.bin",
            "--firmware-version",
            "v1",
            "--logs",
            "--legacy",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
}
#[tokio::test]
async fn disconnect_after_verification_before_commit_never_selects_image() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("disconnect-before-commit").await;
            let error = client_upload(address, true, "disconnect-before-commit")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("commit outcome uncertain"));
            assert_eq!(*state.requests.borrow(), vec!["begin", "chunk", "finish"]);
            assert_eq!(state.commits.get(), 0);
            assert!(!state.pending.get());
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn grpc_preserves_metadata_image_digest_and_order_validation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in [
                "application",
                "chip",
                "version",
                "size",
                "digest-length",
                "descriptor",
                "digest",
                "request-order",
                "early-finish",
            ] {
                let (address, server, state) = fixture("").await;
                let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = Client::new(channel);
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                let bytes = image();
                let Op::Begin(mut metadata) = begin(&bytes, "v1") else {
                    panic!()
                };
                match fault {
                    "application" => metadata.application = "wrong-app".try_into().unwrap(),
                    "chip" => metadata.chip = "esp32".try_into().unwrap(),
                    "version" => metadata.version.clear(),
                    "size" => metadata.image_size = ota_core::SLOT_SIZE + 1,
                    "digest-length" => metadata.sha256.clear(),
                    "digest" => metadata.sha256[0] ^= 1,
                    _ => {}
                }
                let result = op(&tx, &mut replies, Op::Begin(metadata), 1).await;
                if ["application", "chip", "version", "size", "digest-length"].contains(&fault) {
                    assert_eq!(
                        result.unwrap_err().code(),
                        tonic::Code::FailedPrecondition,
                        "{fault}"
                    );
                    assert_eq!(state.writes.get(), 0);
                } else {
                    result.unwrap();
                    let result = match fault {
                        "descriptor" => {
                            let mut bad = bytes.clone();
                            bad[144] ^= 1;
                            op(&tx, &mut replies, chunk(0, &bad), 2).await
                        }
                        "request-order" => op(&tx, &mut replies, chunk(0, &bytes), 3).await,
                        "early-finish" => {
                            op(&tx, &mut replies, Op::Finish(Default::default()), 2).await
                        }
                        "digest" => {
                            op(&tx, &mut replies, chunk(0, &bytes), 2).await.unwrap();
                            op(&tx, &mut replies, Op::Finish(Default::default()), 3).await
                        }
                        _ => unreachable!(),
                    };
                    assert!(result.is_err(), "{fault}");
                    assert_eq!(state.writes.get(), usize::from(fault == "digest"));
                }
                assert_eq!(state.commits.get(), 0);
                assert!(!state.pending.get());
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
