//! Hardware fault harness. Only commit-no-reply sends a valid COMMIT; use a development board.
use ota_protocol::{Request, RequestPayload as Op, Response, ResponsePayload, UPLOAD_VERSION};
fn pause(label: &str) {
    println!("PAUSED: {label}. Perform the manual action, then press Enter to inspect status.");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};
fn exchange(s: &mut TcpStream, id: u32, op: Op) -> Response {
    let r = Request {
        protocol_version: UPLOAD_VERSION,
        request_id: id,
        payload: Some(op),
    };
    let b = ota_protocol::encode_upload(&r).unwrap();
    s.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
    s.write_all(&b).unwrap();
    let mut p = [0; 4];
    s.read_exact(&mut p).unwrap();
    let n = ota_protocol::frame_length(p).unwrap();
    let mut b = vec![0; n];
    s.read_exact(&mut b).unwrap();
    let response: Response = ota_protocol::decode(&b).unwrap();
    assert_eq!(response.protocol_version, UPLOAD_VERSION);
    assert_eq!(response.request_id, id);
    response
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        5,
        "usage: upload-fault-probe IP IMAGE VERSION disconnect|wrong-digest|bad-offset|early-commit|early-finish|bad-id|malformed|oversized|truncated|bad-protobuf|stall|pause-chunk|pause-verified|commit-no-reply"
    );
    let mode = args[4].as_str();
    assert!([
        "disconnect",
        "wrong-digest",
        "bad-offset",
        "early-commit",
        "early-finish",
        "bad-id",
        "oversized",
        "truncated",
        "bad-protobuf",
        "malformed",
        "stall",
        "pause-chunk",
        "pause-verified",
        "commit-no-reply"
    ]
    .contains(&mode));
    let manual = mode.starts_with("pause-") || mode == "commit-no-reply";
    let address: SocketAddr = format!("{}:7779", args[1]).parse().unwrap();
    let image = std::fs::read(&args[2]).unwrap();
    assert!(image.len() <= ota_core::SLOT_SIZE as usize);
    ota_core::validate_image(
        &mut ota_core::SliceImage(&image),
        image.len() as u32,
        &args[3],
    )
    .unwrap();
    let before = espctl::query_status(address, Duration::from_secs(5)).unwrap();
    assert!(before.ota_available);
    std::thread::sleep(Duration::from_millis(250));
    let mut s = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    s.set_nodelay(true).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut digest = ota_core::hash(&image);
    if args[4] == "wrong-digest" {
        digest[0] ^= 1;
    }
    if [
        "malformed",
        "oversized",
        "truncated",
        "bad-protobuf",
        "stall",
    ]
    .contains(&mode)
    {
        if mode == "malformed" {
            s.write_all(&0u32.to_be_bytes()).unwrap();
        } else if mode == "oversized" {
            s.write_all(&513u32.to_be_bytes()).unwrap();
        } else if mode == "bad-protobuf" {
            s.write_all(&1u32.to_be_bytes()).unwrap();
            s.write_all(&[0xff]).unwrap();
        } else if mode == "truncated" {
            s.write_all(&10u32.to_be_bytes()).unwrap();
            s.write_all(&[8]).unwrap();
            s.shutdown(std::net::Shutdown::Write).unwrap();
        } else {
            s.write_all(&[0, 0]).unwrap();
            std::thread::sleep(Duration::from_secs(7));
        }
        let mut byte = [0];
        match s.read(&mut byte) {
            Ok(0) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("session did not demonstrably close: {result:?}"),
        }
        drop(s);
        let after = espctl::query_status(address, Duration::from_secs(7)).unwrap();
        assert_eq!(after.firmware_version, before.firmware_version);
        assert!(after.uptime_ms > before.uptime_ms);
        println!("PASS: invalid session closed and listener recovered");
        return;
    }
    let begin = ota_protocol::BeginUpdate {
        application: "device-demo".try_into().unwrap(),
        chip: "esp32s3".try_into().unwrap(),
        version: args[3].as_str().try_into().unwrap(),
        image_size: image.len() as u32,
        sha256: digest.as_slice().try_into().unwrap(),
    };
    let response = exchange(&mut s, 1, Op::Begin(begin));
    assert!(
        matches!(response.payload, Some(ResponsePayload::Upload(ref a)) if a.phase == ota_protocol::UploadPhase::Receiving && a.next_offset == 0)
    );
    if ["early-commit", "early-finish", "bad-offset", "bad-id"].contains(&mode) {
        let op = if mode == "early-commit" {
            Op::Commit(ota_protocol::CommitUpdate {})
        } else if mode == "early-finish" || mode == "bad-id" {
            Op::Finish(ota_protocol::FinishUpdate {})
        } else {
            Op::Chunk(ota_protocol::FirmwareChunk {
                offset: 1,
                data: image[..image.len().min(4096)].try_into().unwrap(),
            })
        };
        let response = if mode == "bad-id" {
            let request = Request {
                protocol_version: 2,
                request_id: 3,
                payload: Some(op),
            };
            let bytes = ota_protocol::encode_upload(&request).unwrap();
            s.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
            s.write_all(&bytes).unwrap();
            let mut prefix = [0; 4];
            s.read_exact(&mut prefix).unwrap();
            let mut body = vec![0; ota_protocol::frame_length(prefix).unwrap()];
            s.read_exact(&mut body).unwrap();
            let response: Response = ota_protocol::decode(&body).unwrap();
            assert_eq!(response.request_id, 1); // last accepted request
            response
        } else {
            exchange(&mut s, 2, op)
        };
        assert!(matches!(response.payload, Some(ResponsePayload::Error(_))));
        drop(s);
        std::thread::sleep(Duration::from_millis(500));
        let after = espctl::query_status(address, Duration::from_secs(7)).unwrap();
        assert_eq!(after.firmware_version, before.firmware_version);
        assert!(after.uptime_ms > before.uptime_ms);
        println!("PASS: invalid operation rejected without reboot");
        return;
    }
    let mut id = 1;
    for (index, data) in image.chunks(ota_protocol::CHUNK_SIZE).enumerate() {
        id += 1;
        let response = exchange(
            &mut s,
            id,
            Op::Chunk(ota_protocol::FirmwareChunk {
                offset: (index * ota_protocol::CHUNK_SIZE) as u32,
                data: data.try_into().unwrap(),
            }),
        );
        assert!(
            matches!(response.payload, Some(ResponsePayload::Upload(ref a))
            if a.phase == ota_protocol::UploadPhase::Receiving
                && a.next_offset == (index * ota_protocol::CHUNK_SIZE + data.len()) as u32)
        );
        if mode == "pause-chunk" {
            pause("first chunk acknowledged; cut power, reset, or disable the access point now");
            break;
        }
        if mode == "disconnect" {
            break;
        }
    }
    if args[4] == "wrong-digest" {
        let response = exchange(&mut s, id + 1, Op::Finish(ota_protocol::FinishUpdate {}));
        assert!(
            matches!(response.payload,Some(ResponsePayload::Error(ref e)) if e.message.as_str()=="stored image digest mismatch")
        );
        println!("PASS: device rejected wrong image digest");
    }
    if mode == "pause-verified" || mode == "commit-no-reply" {
        let response = exchange(&mut s, id + 1, Op::Finish(ota_protocol::FinishUpdate {}));
        assert!(
            matches!(response.payload, Some(ResponsePayload::Upload(ref a)) if a.phase == ota_protocol::UploadPhase::Verified)
        );
        if mode == "pause-verified" {
            pause("image verified, not committed; power cycle or reset now");
        } else {
            let request = Request {
                protocol_version: 2,
                request_id: id + 2,
                payload: Some(Op::Commit(ota_protocol::CommitUpdate {})),
            };
            let bytes = ota_protocol::encode_upload(&request).unwrap();
            s.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
            s.write_all(&bytes).unwrap();
            // Deliberately do not consume the reply. This does not prove a network packet was dropped.
            pause("COMMIT sent without reading reply; observe reboot, or cut power to probe the commit window");
        }
    }
    drop(s);
    std::thread::sleep(Duration::from_millis(500));
    let after = espctl::query_status(address, Duration::from_secs(7)).unwrap();
    if manual {
        println!("OBSERVATION ONLY: before={} uptime={}ms; after={} uptime={}ms. Check serial boot slot and record outcome; this is not an automatic PASS.", before.firmware_version, before.uptime_ms, after.firmware_version, after.uptime_ms);
        return;
    }
    assert_eq!(after.firmware_version, before.firmware_version);
    assert!(after.uptime_ms > before.uptime_ms);
    println!(
        "PASS: {} left firmware {} running without reboot",
        args[4], after.firmware_version
    );
}
