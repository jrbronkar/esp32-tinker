use ota_protocol::{Request, Response, ResponsePayload, VERSION};
use std::{
    net::{Ipv4Addr, UdpSocket},
    thread,
    time::{Duration, Instant},
};
fn listener() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
}
fn response(id: u32, name: &str) -> Response {
    let mut status = ota_protocol::status(name, "device-demo", "test", 42).unwrap();
    status.management_state = "idle".try_into().unwrap();
    Response {
        protocol_version: VERSION,
        request_id: id,
        payload: Some(ResponsePayload::Status(status)),
    }
}
#[test]
fn collects_multiple_devices_deduplicates_and_ignores_invalid_replies() {
    let socket = listener();
    let address = socket.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut bytes = [0; 512];
        let (n, peer) = socket.recv_from(&mut bytes).unwrap();
        let r: Request = ota_protocol::decode(&bytes[..n]).unwrap();
        assert!(matches!(
            r.payload,
            Some(ota_protocol::RequestPayload::Discovery(_))
        ));
        socket.send_to(&[0xff], peer).unwrap();
        socket.send_to(&[0; 513], peer).unwrap();
        let mut wrong = response(r.request_id.wrapping_add(1), "wrong-id");
        socket
            .send_to(&ota_protocol::encode(&wrong).unwrap(), peer)
            .unwrap();
        wrong.request_id = r.request_id;
        wrong.protocol_version = VERSION + 1;
        socket
            .send_to(&ota_protocol::encode(&wrong).unwrap(), peer)
            .unwrap();
        for name in ["board-b", "board-a", "board-a"] {
            socket
                .send_to(
                    &ota_protocol::encode(&response(r.request_id, name)).unwrap(),
                    peer,
                )
                .unwrap();
        }
        thread::sleep(Duration::from_millis(350));
    });
    let found =
        espctl::discovery::discover(address, Ipv4Addr::LOCALHOST, Duration::from_millis(250))
            .unwrap();
    assert_eq!(found.devices.len(), 2);
    assert_eq!(found.devices[0].status.device_id, "board-a");
    assert_eq!(found.devices[0].address.port(), ota_protocol::PORT);
    assert!(!found.limit_reached);
    server.join().unwrap();
}
#[test]
fn repeats_probe_after_packet_loss_with_same_correlation_id() {
    let socket = listener();
    let address = socket.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut bytes = [0; 512];
        let (n, _) = socket.recv_from(&mut bytes).unwrap();
        let first: Request = ota_protocol::decode(&bytes[..n]).unwrap();
        let (n, peer) = socket.recv_from(&mut bytes).unwrap();
        let second: Request = ota_protocol::decode(&bytes[..n]).unwrap();
        assert_eq!(first, second);
        socket
            .send_to(
                &ota_protocol::encode(&response(second.request_id, "board")).unwrap(),
                peer,
            )
            .unwrap();
        thread::sleep(Duration::from_millis(300));
    });
    assert_eq!(
        espctl::discovery::discover(address, Ipv4Addr::LOCALHOST, Duration::from_millis(700))
            .unwrap()
            .devices
            .len(),
        1
    );
    server.join().unwrap();
}
#[test]
fn empty_scan_is_bounded_and_cli_reports_no_response() {
    let socket = listener();
    let address = socket.local_addr().unwrap();
    let start = Instant::now();
    assert!(
        espctl::discovery::discover(address, Ipv4Addr::LOCALHOST, Duration::from_millis(60))
            .unwrap()
            .devices
            .is_empty()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args([
            "discover",
            "--broadcast",
            "127.0.0.1",
            "--bind",
            "127.0.0.1",
            "--port",
            &address.port().to_string(),
            "--timeout-ms",
            "60",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("No devices responded"));
    assert!(espctl::discovery::discover(address, Ipv4Addr::LOCALHOST, Duration::ZERO).is_err());
}
#[test]
fn bounds_number_of_reported_devices() {
    let socket = listener();
    let address = socket.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut bytes = [0; 512];
        let (n, peer) = socket.recv_from(&mut bytes).unwrap();
        let request: Request = ota_protocol::decode(&bytes[..n]).unwrap();
        for i in 0..70 {
            let name = format!("board-{i}");
            socket
                .send_to(
                    &ota_protocol::encode(&response(request.request_id, &name)).unwrap(),
                    peer,
                )
                .unwrap();
            thread::sleep(Duration::from_millis(3));
        }
        thread::sleep(Duration::from_millis(500));
    });
    let result =
        espctl::discovery::discover(address, Ipv4Addr::LOCALHOST, Duration::from_millis(600))
            .unwrap();
    assert_eq!(result.devices.len(), 64);
    assert!(result.limit_reached);
    server.join().unwrap();
}
