use ota_protocol::{
    ReadLogsRequest, ReadLogsResponse, Request, RequestPayload, Response, ResponsePayload,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::Duration,
};

#[test]
fn log_transport_preserves_cursor_and_rejects_invalid_envelopes() {
    for variant in 0..9 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut prefix = [0; 4];
            socket.read_exact(&mut prefix).unwrap();
            let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
            socket.read_exact(&mut body).unwrap();
            let request: Request = ota_protocol::decode(&body).unwrap();
            assert_eq!(
                request.payload,
                Some(RequestPayload::ReadLogs(ReadLogsRequest {
                    boot_id: 7,
                    after_sequence: 8
                }))
            );
            let mut reply = Response {
                protocol_version: 1,
                request_id: request.request_id,
                payload: Some(ResponsePayload::Logs(ReadLogsResponse {
                    boot_id: 7,
                    next_sequence: 8,
                    ..Default::default()
                })),
            };
            match variant {
                1 => reply.protocol_version += 1,
                2 => reply.request_id += 1,
                3 => reply.payload = None,
                4 => {
                    if let Some(ResponsePayload::Logs(ref mut logs)) = reply.payload {
                        logs.boot_id = 0;
                    }
                }
                5..=8 => {
                    if let Some(ResponsePayload::Logs(ref mut logs)) = reply.payload {
                        match variant {
                            5 => logs.boot_id = 9,       // Must signal a reset on reboot.
                            6 => logs.next_sequence = 9, // No entries to advance over.
                            7 => {
                                for _ in 0..4 {
                                    logs.entries
                                        .push(ota_protocol::LogEntry {
                                            sequence: 8,
                                            ..Default::default()
                                        })
                                        .unwrap();
                                }
                            }
                            8 => {
                                for sequence in [10, 9] {
                                    logs.entries
                                        .push(ota_protocol::LogEntry {
                                            sequence,
                                            ..Default::default()
                                        })
                                        .unwrap();
                                }
                                logs.next_sequence = 9;
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                _ => {}
            }
            let bytes = ota_protocol::encode(&reply).unwrap();
            // Exercise fragmented framing on the new reader.
            for byte in (bytes.len() as u32)
                .to_be_bytes()
                .iter()
                .chain(bytes.iter())
            {
                socket.write_all(&[*byte]).unwrap();
            }
        });
        let result = espctl::read_logs(
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
