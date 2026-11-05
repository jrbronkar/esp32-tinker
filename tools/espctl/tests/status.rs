#[path = "support/server.rs"]
mod server;
use ota_protocol::ResponsePayload;
use server::*;
use std::{
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};
fn mock(
    action: impl FnMut(ota_protocol::Request) -> Result<Vec<u8>, protolink::Status> + Send + 'static,
) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    spawn(1, usize::MAX, Duration::ZERO, action)
}
#[test]
fn accepts_fragmented_http2_response() {
    let (address, handle) = spawn(1, 1, Duration::ZERO, |request| Ok(bytes(&reply(request))));
    let status = espctl::query_status(address, Duration::from_secs(2)).unwrap();
    assert_eq!(status.device_id, "mock-board");
    assert_eq!(status.uptime_ms, 1234);
    handle.join().unwrap();
}
#[test]
fn rejects_oversized_empty_and_malformed_protobuf() {
    for body in [vec![], vec![0xff], vec![0; 513]] {
        let (address, handle) = mock(move |_| Ok(body.clone()));
        assert!(espctl::query_status(address, Duration::from_secs(2)).is_err());
        handle.join().unwrap();
    }
}
#[test]
fn rejects_mismatched_version_id_and_missing_identity_or_payload() {
    for variant in 0..4 {
        let (address, handle) = mock(move |r| {
            let mut response = reply(r);
            match variant {
                0 => response.protocol_version += 1,
                1 => response.request_id += 1,
                2 => response.payload = None,
                _ => {
                    if let Some(ResponsePayload::Status(s)) = &mut response.payload {
                        s.device_id.clear();
                    }
                }
            }
            Ok(bytes(&response))
        });
        assert!(espctl::query_status(address, Duration::from_secs(2)).is_err());
        handle.join().unwrap();
    }
}
#[test]
fn surfaces_grpc_error() {
    let (address, handle) = mock(|_| {
        Err(protolink::Status::invalid_argument(
            "unsupported protocol version",
        ))
    });
    assert!(espctl::query_status(address, Duration::from_secs(2))
        .unwrap_err()
        .to_string()
        .contains("unsupported protocol version"));
    handle.join().unwrap();
}
#[test]
fn total_deadline_expires_even_with_trickling_http2_bytes() {
    let (address, handle) = spawn(1, 1, Duration::from_millis(40), |r| Ok(bytes(&reply(r))));
    let start = Instant::now();
    assert!(espctl::query_status(address, Duration::from_millis(120)).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    handle.join().unwrap();
}
#[test]
fn cli_prints_status_and_can_reconnect() {
    let (address, handle) = spawn(2, usize::MAX, Duration::ZERO, |r| {
        thread::sleep(Duration::from_millis(25));
        Ok(bytes(&reply(r)))
    });
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
            .args([
                "status",
                "--host",
                "127.0.0.1",
                "--port",
                &address.port().to_string(),
                "--timings",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in [
            "mock-board",
            "test-v1",
            "unavailable (unsupported boot layout)",
            "RPC response:",
            "0 refusals",
        ] {
            assert!(text.contains(expected), "{text}");
        }
    }
    handle.join().unwrap();
}
#[test]
fn cli_bad_arguments_and_connection_failure_are_nonzero() {
    for args in [
        vec!["status"],
        vec!["status", "--host", "bad"],
        vec!["status", "--host", "127.0.0.1", "--port", "0"],
        vec!["status", "--host", "127.0.0.1", "--timeout-ms", "0"],
        vec!["status", "--host", "127.0.0.1", "--timeout-ms", "60001"],
    ] {
        assert!(!Command::new(env!("CARGO_BIN_EXE_espctl"))
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(espctl::query_status(address, Duration::from_millis(150)).is_err());
    assert!(espctl::query_status(address, Duration::ZERO).is_err());
    assert!(espctl::query_status(address, Duration::MAX).is_err());
}
#[test]
fn retries_refused_connect_until_listener_returns() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        run(
            TcpListener::bind(address).unwrap(),
            1,
            usize::MAX,
            Duration::ZERO,
            |r| Ok(bytes(&reply(r))),
        )
    });
    let result = espctl::query_status_profiled(address, Duration::from_secs(2)).unwrap();
    assert!(result.timings.refused_connects > 0);
    assert!(result.timings.retry_wait >= Duration::from_millis(50));
    handle.join().unwrap();
}
#[test]
fn cli_help_and_version_succeed_without_connecting() {
    for args in [vec!["--help"], vec!["status", "--help"], vec!["--version"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
}
#[test]
fn profiling_attributes_delayed_response_to_rpc() {
    let (address, handle) = mock(|r| {
        thread::sleep(Duration::from_millis(30));
        Ok(bytes(&reply(r)))
    });
    let t = espctl::query_status_profiled(address, Duration::from_secs(2))
        .unwrap()
        .timings;
    assert!(t.rpc >= Duration::from_millis(25));
    assert_eq!(t.refused_connects, 0);
    assert!(t.total >= t.connect + t.encode + t.rpc + t.decode_validate);
    handle.join().unwrap();
}
#[test]
fn profile_produces_summary_and_csv_for_multiple_queries() {
    for csv in [false, true] {
        let (address, handle) = spawn(3, usize::MAX, Duration::ZERO, |r| Ok(bytes(&reply(r))));
        let mut command = Command::new(env!("CARGO_BIN_EXE_espctl"));
        command.args([
            "profile",
            "--host",
            "127.0.0.1",
            "--port",
            &address.port().to_string(),
            "--samples",
            "3",
            "--interval-ms",
            "0",
        ]);
        if csv {
            command.arg("--csv");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        if csv {
            assert_eq!(text.lines().count(), 4);
            assert!(text.starts_with("sample,total_ms,connect_ms,"));
            for line in text.lines().skip(1) {
                assert_eq!(line.split(',').count(), 10);
            }
        } else {
            assert!(text.contains("3 successful, 0 failed"));
            assert!(text.contains("rpc"));
        }
        handle.join().unwrap();
    }
}
#[test]
fn optional_device_timings_are_requested_and_preserved() {
    let (address, handle) = mock(|r| {
        assert!(
            matches!(&r.payload,Some(ota_protocol::RequestPayload::Status(s)) if s.include_timings)
        );
        let mut response = reply(r);
        if let Some(ResponsePayload::Status(s)) = &mut response.payload {
            s.set_timings(ota_protocol::DeviceTimings {
                request_read_us: 0,
                decode_us: 12,
                prepare_us: 4,
            });
        }
        Ok(bytes(&response))
    });
    let query = espctl::query_status_profiled(address, Duration::from_secs(2)).unwrap();
    assert_eq!(query.status.timings().unwrap().decode_us, 12);
    handle.join().unwrap();
}
#[test]
fn ordinary_queries_do_not_request_device_tracing() {
    let (address, handle) = mock(|r| {
        assert!(
            matches!(&r.payload,Some(ota_protocol::RequestPayload::Status(s)) if !s.include_timings)
        );
        Ok(bytes(&reply(r)))
    });
    assert!(espctl::query_status(address, Duration::from_secs(2))
        .unwrap()
        .timings()
        .is_none());
    handle.join().unwrap();
}
#[test]
fn boot_inventory_is_requested_and_displayed_without_claiming_ota_support() {
    let (address, handle) = mock(|r| {
        assert!(
            matches!(&r.payload,Some(ota_protocol::RequestPayload::Status(s)) if s.include_boot)
        );
        let mut response = reply(r);
        if let Some(ResponsePayload::Status(s)) = &mut response.payload {
            let mut boot = ota_protocol::BootInfo {
                configured_flash_size_bytes: 16 * 1024 * 1024,
                partition_count: 3,
                layout_valid: true,
                ..Default::default()
            };
            boot.set_running(ota_protocol::PartitionInfo {
                label: "factory".try_into().unwrap(),
                offset: 0x10000,
                size: 0x300000,
            });
            s.set_boot(boot);
        }
        Ok(bytes(&response))
    });
    let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args([
            "status",
            "--host",
            "127.0.0.1",
            "--port",
            &address.port().to_string(),
            "--boot",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "16777216 bytes (image header)",
        "factory offset=0x10000 size=0x300000",
        "OTA app slots: 0",
        "rollback support: unverified",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    handle.join().unwrap();
}
