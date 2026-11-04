use ota_protocol::{
    Request, RequestPayload as Op, Response, ResponsePayload, UploadAck, UploadPhase,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::Duration,
};
fn image() -> Vec<u8> {
    let mut b = vec![0u8; 336];
    b[0] = 0xe9;
    b[1] = 1;
    b[12] = 9;
    b[23] = 1;
    b[28..32].copy_from_slice(&256u32.to_le_bytes());
    b[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
    b[48..50].copy_from_slice(b"v1");
    b[80..91].copy_from_slice(b"device-demo");
    b[144..144 + ota_core::MARKER.len()].copy_from_slice(ota_core::MARKER.as_bytes());
    b[303] = b[32..288].iter().fold(0xef, |a, v| a ^ v);
    let hash = ota_core::hash(&b[..304]);
    b[304..].copy_from_slice(&hash);
    b
}
fn request(s: &mut TcpStream) -> Request {
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut p = [0; 4];
    s.read_exact(&mut p).unwrap();
    let n = ota_protocol::upload_frame_length(p).unwrap();
    let mut body = vec![0; n];
    s.read_exact(&mut body).unwrap();
    ota_protocol::decode_upload(&body).unwrap()
}
fn send(s: &mut TcpStream, r: Response) {
    let body = ota_protocol::encode(&r).unwrap();
    s.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
    s.write_all(&body).unwrap();
}
fn ack(s: &mut TcpStream, id: u32, phase: UploadPhase, n: u32) {
    send(
        s,
        Response {
            protocol_version: 2,
            request_id: id,
            payload: Some(ResponsePayload::Upload(UploadAck {
                phase,
                next_offset: n,
                target_offset: 0x310000,
            })),
        },
    );
}
#[test]
fn upload_verifies_and_activates_only_when_explicitly_requested() {
    for commit in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let bytes = image();
        let expected = bytes.clone();
        let file: PathBuf =
            std::env::temp_dir().join(format!("espctl-image-{}-{commit}.bin", std::process::id()));
        std::fs::write(&file, &bytes).unwrap();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let r = request(&mut s);
            let mut status = ota_protocol::status("mock", "device-demo", "old", 1).unwrap();
            status.ota_available = true;
            send(&mut s, ota_protocol::respond(r, status));
            drop(s);
            let (mut s, _) = listener.accept().unwrap();
            let r = request(&mut s);
            assert!(
                matches!(&r.payload,Some(Op::Begin(m)) if m.image_size==expected.len() as u32 && m.sha256.as_slice()==ota_core::hash(&expected))
            );
            ack(&mut s, r.request_id, UploadPhase::Receiving, 0);
            let r = request(&mut s);
            assert!(
                matches!(&r.payload,Some(Op::Chunk(c)) if c.offset==0 && c.data.as_slice()==expected)
            );
            ack(
                &mut s,
                r.request_id,
                UploadPhase::Receiving,
                expected.len() as u32,
            );
            let r = request(&mut s);
            assert!(matches!(r.payload, Some(Op::Finish(_))));
            ack(
                &mut s,
                r.request_id,
                UploadPhase::Verified,
                expected.len() as u32,
            );
            let r = request(&mut s);
            if commit {
                assert!(matches!(r.payload, Some(Op::Commit(_))));
                ack(
                    &mut s,
                    r.request_id,
                    UploadPhase::Committed,
                    expected.len() as u32,
                );
            } else {
                assert!(matches!(r.payload, Some(Op::Abort(_))));
                ack(
                    &mut s,
                    r.request_id,
                    UploadPhase::Aborted,
                    expected.len() as u32,
                );
            }
        });
        let result =
            espctl::upload::upload(address, Duration::from_secs(3), &file, "v1", commit).unwrap();
        assert_eq!(result.committed, commit);
        assert_eq!(result.bytes, bytes.len() as u32);
        handle.join().unwrap();
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn upload_rejects_bad_acknowledgements_and_does_not_replay() {
    for fault in [
        "id",
        "version",
        "phase",
        "offset",
        "target",
        "error",
        "truncated",
        "disconnect",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let file =
            std::env::temp_dir().join(format!("espctl-fault-{}-{fault}.bin", std::process::id()));
        std::fs::write(&file, image()).unwrap();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let r = request(&mut s);
            let mut status = ota_protocol::status("mock", "device-demo", "old", 1).unwrap();
            status.ota_available = true;
            send(&mut s, ota_protocol::respond(r, status));
            drop(s);
            let (mut s, _) = listener.accept().unwrap();
            let r = request(&mut s);
            assert!(matches!(r.payload, Some(Op::Begin(_))));
            if fault == "truncated" {
                s.write_all(&10u32.to_be_bytes()).unwrap();
                s.write_all(&[8]).unwrap();
            } else if fault != "disconnect" {
                send(
                    &mut s,
                    Response {
                        protocol_version: if fault == "version" { 1 } else { 2 },
                        request_id: r.request_id + u32::from(fault == "id"),
                        payload: Some(if fault == "error" {
                            ResponsePayload::Error(ota_protocol::ErrorResponse {
                                code: ota_protocol::ErrorCode::InvalidRequest,
                                message: "injected failure".try_into().unwrap(),
                            })
                        } else {
                            ResponsePayload::Upload(UploadAck {
                                phase: if fault == "phase" {
                                    UploadPhase::Verified
                                } else {
                                    UploadPhase::Receiving
                                },
                                next_offset: u32::from(fault == "offset"),
                                target_offset: if fault == "target" { 0x9000 } else { 0x310000 },
                            })
                        }),
                    },
                );
                let mut byte = [0];
                assert_eq!(
                    s.read(&mut byte).unwrap(),
                    0,
                    "client must stop after invalid ack"
                );
            }
            drop(s);
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        assert!(
            espctl::upload::upload(address, Duration::from_secs(2), &file, "v1", true).is_err(),
            "{fault}"
        );
        handle.join().unwrap();
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn lost_commit_reply_reports_uncertainty_without_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let file = std::env::temp_dir().join(format!("espctl-lost-commit-{}.bin", std::process::id()));
    let bytes = image();
    let size = bytes.len() as u32;
    std::fs::write(&file, bytes).unwrap();
    let handle = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let r = request(&mut s);
        let mut status = ota_protocol::status("mock", "device-demo", "old", 1).unwrap();
        status.ota_available = true;
        send(&mut s, ota_protocol::respond(r, status));
        drop(s);
        let (mut s, _) = listener.accept().unwrap();
        for (phase, n) in [
            (UploadPhase::Receiving, 0),
            (UploadPhase::Receiving, size),
            (UploadPhase::Verified, size),
        ] {
            let r = request(&mut s);
            ack(&mut s, r.request_id, phase, n);
        }
        assert!(matches!(request(&mut s).payload, Some(Op::Commit(_))));
        drop(s); // Models metadata committed, but acknowledgement lost.
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let error =
        espctl::upload::upload(address, Duration::from_secs(2), &file, "v1", true).unwrap_err();
    assert!(error.to_string().contains("commit outcome uncertain"));
    handle.join().unwrap();
    std::fs::remove_file(file).unwrap();
}
