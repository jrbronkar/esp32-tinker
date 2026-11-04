use ota_protocol::{Request, Response, ResponsePayload};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn mock(action: impl FnOnce(TcpStream) + Send + 'static) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        action(socket);
    });
    (address, handle)
}

fn request(socket: &mut TcpStream) -> Request {
    let mut prefix = [0; 4];
    socket.read_exact(&mut prefix).unwrap();
    let len = ota_protocol::frame_length(prefix).unwrap();
    let mut body = vec![0; len];
    socket.read_exact(&mut body).unwrap();
    ota_protocol::decode(&body).unwrap()
}

fn reply(request: Request) -> Response {
    ota_protocol::respond(
        request,
        ota_protocol::status("mock-board", "device-demo", "test-v1", 1234).unwrap(),
    )
}

fn send(socket: &mut TcpStream, response: &Response) {
    let body = ota_protocol::encode(response).unwrap();
    socket
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&body).unwrap();
}

#[test]
fn accepts_fragmented_prefix_and_body() {
    let (address, handle) = mock(|mut socket| {
        let body = ota_protocol::encode(&reply(request(&mut socket))).unwrap();
        for byte in (body.len() as u32).to_be_bytes().iter().chain(body.iter()) {
            socket.write_all(&[*byte]).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });
    let result = espctl::query_status(address, Duration::from_secs(2)).unwrap();
    assert_eq!(result.device_id.as_str(), "mock-board");
    assert_eq!(result.uptime_ms, 1234);
    assert!(!result.ota_available);
    handle.join().unwrap();
}

#[test]
fn rejects_bad_lengths_truncation_and_malformed_body() {
    for bytes in [
        0u32.to_be_bytes().to_vec(),
        u32::MAX.to_be_bytes().to_vec(),
        vec![0, 0, 0, 10, 8],  // short body
        vec![0, 0, 0, 1, 255], // invalid protobuf
    ] {
        let (address, handle) = mock(move |mut socket| {
            request(&mut socket);
            socket.write_all(&bytes).unwrap();
        });
        assert!(espctl::query_status(address, Duration::from_secs(2)).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn rejects_mismatched_version_id_and_missing_payload() {
    for variant in 0..3 {
        let (address, handle) = mock(move |mut socket| {
            let mut response = reply(request(&mut socket));
            match variant {
                0 => response.protocol_version += 1,
                1 => response.request_id += 1,
                _ => response.payload = None,
            }
            send(&mut socket, &response);
        });
        assert!(espctl::query_status(address, Duration::from_secs(2)).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn surfaces_device_error() {
    let (address, handle) = mock(|mut socket| {
        let mut req = request(&mut socket);
        req.protocol_version = 99;
        let response = reply(req);
        assert!(matches!(response.payload, Some(ResponsePayload::Error(_))));
        send(&mut socket, &response);
    });
    assert!(espctl::query_status(address, Duration::from_secs(2))
        .unwrap_err()
        .to_string()
        .contains("unsupported protocol version"));
    handle.join().unwrap();
}

#[test]
fn total_deadline_expires_even_with_trickling_bytes() {
    let (address, handle) = mock(|mut socket| {
        request(&mut socket);
        for byte in [0, 0, 0, 100, 8, 1] {
            if socket.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let start = Instant::now();
    assert!(espctl::query_status(address, Duration::from_millis(120)).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    handle.join().unwrap();
}

#[test]
fn cli_prints_status_and_can_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let response = reply(request(&mut socket));
            thread::sleep(Duration::from_millis(25));
            send(&mut socket, &response);
        }
    });
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
            .args([
                "status",
                "--host",
                "127.0.0.1",
                "--port",
                &port,
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
        assert!(text.contains("mock-board"));
        assert!(text.contains("test-v1"));
        assert!(text.contains("unavailable (status only)"));
        assert!(text.contains("First byte:"));
        assert!(text.contains("0 refusals"));
        let latency = text
            .lines()
            .find_map(|line| line.strip_prefix("Latency:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(latency.is_finite() && latency >= 20.0, "{text}");
    }
    handle.join().unwrap();
}

#[test]
fn cli_bad_arguments_and_connection_failure_are_nonzero() {
    for args in [
        vec!["status"],
        vec!["status", "--host", "bad"],
        vec!["status", "--host"],
        vec!["status", "--host", "127.0.0.1", "--port", "0"],
        vec!["status", "--host", "127.0.0.1", "--timeout-ms", "60001"],
        vec!["status", "--host", "127.0.0.1", "--host", "127.0.0.2"],
        vec!["status", "--host", "127.0.0.1", "--timeout-ms", "0"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_espctl"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(espctl::query_status(address, Duration::from_millis(500)).is_err());
}

#[test]
fn retries_refused_connect_until_listener_returns() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let listener = TcpListener::bind(address).unwrap();
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let response = reply(request(&mut socket));
        send(&mut socket, &response);
    });
    let status = espctl::query_status(address, Duration::from_secs(2)).unwrap();
    assert_eq!(status.application.as_str(), "device-demo");
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
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn profiling_attributes_delayed_response_to_first_byte() {
    let (address, handle) = mock(|mut socket| {
        let response = reply(request(&mut socket));
        thread::sleep(Duration::from_millis(30));
        send(&mut socket, &response);
    });
    let query = espctl::query_status_profiled(address, Duration::from_secs(2)).unwrap();
    let t = query.timings;
    assert!(t.first_byte >= Duration::from_millis(25));
    assert_eq!(t.refused_connects, 0);
    assert_eq!(t.retry_wait, Duration::ZERO);
    assert!(t.total >= t.connect + t.write + t.first_byte + t.read + t.decode_validate);
    handle.join().unwrap();
}

#[test]
fn profile_produces_summary_and_csv_for_multiple_queries() {
    for csv in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let handle = thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let response = reply(request(&mut socket));
                send(&mut socket, &response);
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_espctl"));
        command.args([
            "profile",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
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
                let fields: Vec<_> = line.split(',').collect();
                assert_eq!(fields.len(), 12);
                assert!(fields[1].parse::<f64>().unwrap() > 0.0);
            }
        } else {
            assert!(text.contains("3 successful, 0 failed"));
            assert!(text.contains("median"));
            assert!(text.contains("first byte"));
        }
        handle.join().unwrap();
    }
}

#[test]
fn optional_device_timings_are_requested_and_preserved() {
    let (address, handle) = mock(|mut socket| {
        let request = request(&mut socket);
        assert!(
            matches!(&request.payload, Some(ota_protocol::RequestPayload::Status(s)) if s.include_timings)
        );
        let mut response = reply(request);
        if let Some(ResponsePayload::Status(status)) = &mut response.payload {
            status.set_timings(ota_protocol::DeviceTimings {
                request_read_us: 1200,
                decode_us: 12,
                prepare_us: 4,
            });
        }
        send(&mut socket, &response);
    });
    let query = espctl::query_status_profiled(address, Duration::from_secs(2)).unwrap();
    let timing = query.status.timings().unwrap();
    assert_eq!(timing.request_read_us, 1200);
    assert_eq!(timing.decode_us, 12);
    assert_eq!(timing.prepare_us, 4);
    handle.join().unwrap();
}

#[test]
fn ordinary_queries_do_not_request_device_tracing() {
    let (address, handle) = mock(|mut socket| {
        let request = request(&mut socket);
        assert!(
            matches!(&request.payload, Some(ota_protocol::RequestPayload::Status(s)) if !s.include_timings)
        );
        send(&mut socket, &reply(request));
    });
    assert!(espctl::query_status(address, Duration::from_secs(2))
        .unwrap()
        .timings()
        .is_none());
    handle.join().unwrap();
}

#[test]
fn boot_inventory_is_requested_and_displayed_without_claiming_ota_support() {
    let (address, handle) = mock(|mut socket| {
        let request = request(&mut socket);
        assert!(
            matches!(&request.payload, Some(ota_protocol::RequestPayload::Status(s)) if s.include_boot)
        );
        let mut response = reply(request);
        if let Some(ResponsePayload::Status(status)) = &mut response.payload {
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
            status.set_boot(boot);
        }
        send(&mut socket, &response);
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
    assert!(text.contains("16777216 bytes (image header)"));
    assert!(text.contains("factory offset=0x10000 size=0x300000"));
    assert!(text.contains("OTA app slots: 0"));
    assert!(text.contains("rollback support: unverified"));
    handle.join().unwrap();
}
