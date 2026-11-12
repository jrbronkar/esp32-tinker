use std::{vec, vec::Vec};

use crate::protocol::{self, ACK_FRAME_LEN, KIND_ACK, KIND_DATA, Packet};
use crate::{max_encoding_length, sync};

const SESSION: u64 = 0x0102_0304_0506_0708;

fn seal(kind: u8, session: u64, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut decoded = vec![kind];
    decoded.extend_from_slice(&session.to_be_bytes());
    decoded.extend_from_slice(&seq.to_be_bytes());
    decoded.extend_from_slice(payload);
    let crc = protocol::crc32(&decoded);
    decoded.extend_from_slice(&crc.to_be_bytes());
    decoded
}

fn wire(decoded: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; max_encoding_length(decoded.len()) + 2];
    let len = sync::encode_from_slice_including_sentinels(decoded, &mut frame).unwrap();
    frame.truncate(len);
    frame
}

fn unwire(frame: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = vec![0u8; frame.len()];
    let result = sync::decode_to_slice(frame, &mut decoded).ok()?;
    decoded.truncate(result.len);
    Some(decoded)
}

#[test]
fn crc32_matches_check_value() {
    assert_eq!(protocol::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(protocol::crc32(b""), 0);
}

#[test]
fn max_payload_is_the_largest_fitting_payload() {
    assert_eq!(protocol::max_payload(0), 0);
    assert_eq!(protocol::max_payload(20), 0);
    assert_eq!(protocol::max_payload(21), 1);
    for capacity in 0..2048 {
        let payload = protocol::max_payload(capacity);
        if payload == 0 {
            continue;
        }
        let fits = |payload: usize| max_encoding_length(payload + 17) + 2 <= capacity;
        assert!(fits(payload), "capacity {capacity}");
        assert!(!fits(payload + 1), "capacity {capacity}");
    }
}

#[test]
fn data_round_trips_at_maximum_payload() {
    for capacity in [21, 64, 256, 300, 1024] {
        let max = protocol::max_payload(capacity);
        let payload: Vec<u8> = (0..max).map(|i| (i % 3) as u8).collect();
        let mut scratch = vec![0u8; capacity];
        let mut frame = vec![0u8; capacity];
        let len = protocol::encode_data(SESSION, 77, &payload, &mut scratch, &mut frame);
        assert!(len <= capacity);
        assert_eq!(frame[0], 0);
        assert_eq!(frame[len - 1], 0);
        assert!(frame[1..len - 1].iter().all(|&byte| byte != 0));
        let decoded = unwire(&frame[..len]).unwrap();
        match protocol::parse(&decoded) {
            Some(Packet::Data {
                session,
                seq,
                payload: parsed,
            }) => {
                assert_eq!((session, seq), (SESSION, 77));
                assert_eq!(parsed, payload);
            }
            _ => panic!("expected DATA"),
        }
    }
}

#[test]
fn ack_round_trips() {
    let mut frame = [0u8; ACK_FRAME_LEN];
    let len = protocol::encode_ack(SESSION, u32::MAX, &mut frame);
    let decoded = unwire(&frame[..len]).unwrap();
    assert!(matches!(
        protocol::parse(&decoded),
        Some(Packet::Ack {
            session: SESSION,
            seq: u32::MAX
        })
    ));
}

#[test]
fn length_rules_are_strict() {
    assert!(protocol::parse(&seal(KIND_DATA, SESSION, 0, &[])).is_none());
    assert!(protocol::parse(&seal(KIND_ACK, SESSION, 0, &[1])).is_none());
    for kind in [0, 3, 255] {
        assert!(protocol::parse(&seal(kind, SESSION, 0, &[1])).is_none());
        assert!(protocol::parse(&seal(kind, SESSION, 0, &[])).is_none());
    }
    let valid = seal(KIND_DATA, SESSION, 0, &[1, 2, 3]);
    for len in 0..valid.len() {
        assert!(protocol::parse(&valid[..len]).is_none(), "len {len}");
    }
    let mut longer = valid.clone();
    longer.push(0);
    assert!(protocol::parse(&longer).is_none());
}

#[test]
fn any_single_bit_corruption_is_rejected() {
    for decoded in [
        seal(KIND_DATA, SESSION, 5, &[0, 1, 2, 3, 0, 0, 9]),
        seal(KIND_ACK, SESSION, 5, &[]),
    ] {
        assert!(protocol::parse(&decoded).is_some());
        for byte in 0..decoded.len() {
            for bit in 0..8 {
                let mut corrupted = decoded.clone();
                corrupted[byte] ^= 1 << bit;
                assert!(
                    protocol::parse(&corrupted).is_none(),
                    "byte {byte} bit {bit}"
                );
            }
        }
    }
}

#[test]
fn corrupted_wire_bytes_never_parse() {
    let decoded = seal(KIND_DATA, SESSION, 1, &[10, 0, 20, 0, 30]);
    let frame = wire(&decoded);
    for index in 1..frame.len() - 1 {
        for replacement in 1..=255u8 {
            if frame[index] == replacement {
                continue;
            }
            let mut corrupted = frame.clone();
            corrupted[index] = replacement;
            if let Some(decoded) = unwire(&corrupted) {
                assert!(
                    protocol::parse(&decoded).is_none(),
                    "index {index} value {replacement}"
                );
            }
        }
    }
}
