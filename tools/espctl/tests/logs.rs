use ota_protocol::{ReadLogsRequest, ReadLogsResponse};
use std::{process::Command, thread, time::Duration};
#[path = "support/server.rs"]
mod server;

fn read_logs(
    address: std::net::SocketAddr,
    timeout: Duration,
    mut cursor: ReadLogsRequest,
) -> std::io::Result<()> {
    espctl::stream_logs(address, timeout, false, &mut cursor, |_| {})
}

#[test]
fn log_transport_preserves_cursor_and_rejects_invalid_batches() {
    for variant in 0..6 {
        let (address, handle) = server::spawn_logs(1, 1, Duration::ZERO, move |request| {
            assert_eq!(
                request,
                ReadLogsRequest {
                    boot_id: 7,
                    after_sequence: 8
                }
            );
            let mut logs = ReadLogsResponse {
                boot_id: 7,
                next_sequence: 8,
                ..Default::default()
            };
            match variant {
                1 => logs.boot_id = 0,
                2 => logs.boot_id = 9,
                3 => logs.next_sequence = 9,
                4 | 5 => {
                    for sequence in if variant == 4 { [8, 8] } else { [10, 9] } {
                        logs.entries
                            .push(ota_protocol::LogEntry {
                                sequence,
                                ..Default::default()
                            })
                            .unwrap();
                    }
                    logs.next_sequence = 9;
                }
                _ => {}
            }
            Ok(ota_protocol::encode(&logs).unwrap().to_vec())
        });
        let result = read_logs(
            address,
            Duration::from_secs(2),
            ReadLogsRequest {
                boot_id: 7,
                after_sequence: 8,
            },
        );
        assert_eq!(result.is_ok(), variant == 0);
        handle.join().unwrap();
    }
}

#[test]
fn cli_exposes_log_follow_and_requires_target() {
    let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args(["logs", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--follow"));
    assert!(!Command::new(env!("CARGO_BIN_EXE_espctl"))
        .arg("logs")
        .output()
        .unwrap()
        .status
        .success());
}

fn adversarial_server(
    bytes: Vec<u8>,
    trickle: bool,
) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    server::spawn_logs(
        1,
        if trickle { 1 } else { usize::MAX },
        if trickle {
            Duration::from_millis(40)
        } else {
            Duration::ZERO
        },
        move |_| Ok(bytes.clone()),
    )
}

#[test]
fn logs_reject_hostile_framing_and_truncated_protobuf() {
    for bytes in [vec![], vec![0; 513], vec![8], vec![255]] {
        let (address, handle) = adversarial_server(bytes, false);
        assert!(read_logs(address, Duration::from_secs(2), ReadLogsRequest::default()).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn logs_total_deadline_bounds_a_trickling_peer() {
    let (address, handle) = adversarial_server(vec![0, 0, 0, 100, 8, 1, 16, 1, 98, 90], true);
    let start = std::time::Instant::now();
    assert!(read_logs(
        address,
        Duration::from_millis(150),
        ReadLogsRequest::default()
    )
    .is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    handle.join().unwrap();
}

#[test]
fn cli_escapes_remote_terminal_control_sequences() {
    let mut logs = ReadLogsResponse {
        boot_id: 7,
        next_sequence: 1,
        ..Default::default()
    };
    logs.entries
        .push(ota_protocol::LogEntry {
            sequence: 1,
            message: "\x1b[2J\r\nforged line".try_into().unwrap(),
            ..Default::default()
        })
        .unwrap();
    let response = logs;
    let body = ota_protocol::encode(&response).unwrap();
    let bytes = body.to_vec();
    let (address, handle) = adversarial_server(bytes, false);
    let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args([
            "logs",
            "--host",
            "127.0.0.1",
            "--port",
            &address.port().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\x1b'));
    assert!(!text.contains('\r'));
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("\\r\\nforged line"));
    handle.join().unwrap();
}

#[test]
fn follow_resumes_with_the_last_delivered_cursor() {
    let mut sequence = 0;
    let (address, handle) = server::spawn_logs(2, usize::MAX, Duration::ZERO, move |request| {
        assert_eq!(request.after_sequence, sequence);
        assert_eq!(request.boot_id, if sequence == 0 { 0 } else { 7 });
        sequence += 1;
        let mut batch = ReadLogsResponse {
            boot_id: 7,
            next_sequence: sequence,
            ..Default::default()
        };
        batch
            .entries
            .push(ota_protocol::LogEntry {
                sequence,
                ..Default::default()
            })
            .unwrap();
        Ok(server::bytes(&batch))
    });
    let mut cursor = ReadLogsRequest::default();
    let mut received = Vec::new();
    for _ in 0..2 {
        espctl::stream_logs(
            address,
            Duration::from_secs(2),
            true,
            &mut cursor,
            |batch| {
                received.push(batch.entries[0].sequence);
            },
        )
        .unwrap();
    }
    assert_eq!(received, [1, 2]);
    handle.join().unwrap();
}
