use crate::{max_encoding_length, sync};

pub(crate) const KIND_DATA: u8 = 1;
pub(crate) const KIND_ACK: u8 = 2;

const HEADER_LEN: usize = 1 + 8 + 4;
const CRC_LEN: usize = 4;
const OVERHEAD: usize = HEADER_LEN + CRC_LEN;

pub(crate) const ACK_FRAME_LEN: usize = max_encoding_length(OVERHEAD) + 2;

pub(crate) enum Packet<'a> {
    Data {
        session: u64,
        seq: u32,
        payload: &'a [u8],
    },
    Ack {
        session: u64,
        seq: u32,
    },
}

/// CRC-32/ISO-HDLC: polynomial 0x04C11DB7 reflected, init and xorout 0xFFFFFFFF.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub(crate) fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |crc, &byte| {
        CRC_TABLE[usize::from((crc as u8) ^ byte)] ^ (crc >> 8)
    })
}

/// Largest payload whose delimited frame fits in `capacity` bytes, or zero if
/// no payload does.
pub(crate) const fn max_payload(capacity: usize) -> usize {
    let Some(body) = capacity.checked_sub(2) else {
        return 0;
    };
    let mut len = body;
    while len > OVERHEAD && max_encoding_length(len) > body {
        len -= 1;
    }
    len.saturating_sub(OVERHEAD)
}

fn seal(decoded: &mut [u8], len: usize, frame: &mut [u8]) -> usize {
    let crc = crc32(&decoded[..len - CRC_LEN]);
    decoded[len - CRC_LEN..len].copy_from_slice(&crc.to_be_bytes());
    sync::encode_from_slice_including_sentinels(&decoded[..len], frame)
        .expect("frame buffer holds the encoded packet")
}

fn header(decoded: &mut [u8], kind: u8, session: u64, seq: u32) {
    decoded[0] = kind;
    decoded[1..9].copy_from_slice(&session.to_be_bytes());
    decoded[9..HEADER_LEN].copy_from_slice(&seq.to_be_bytes());
}

/// Encodes a delimited DATA frame into `frame`, using `scratch` for the
/// unencoded packet. Returns the frame length.
pub(crate) fn encode_data(
    session: u64,
    seq: u32,
    payload: &[u8],
    scratch: &mut [u8],
    frame: &mut [u8],
) -> usize {
    header(scratch, KIND_DATA, session, seq);
    scratch[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    seal(scratch, OVERHEAD + payload.len(), frame)
}

/// Encodes a delimited ACK frame into `frame`. Returns the frame length.
pub(crate) fn encode_ack(session: u64, seq: u32, frame: &mut [u8; ACK_FRAME_LEN]) -> usize {
    let mut decoded = [0u8; OVERHEAD];
    header(&mut decoded, KIND_ACK, session, seq);
    seal(&mut decoded, OVERHEAD, frame)
}

pub(crate) fn parse(decoded: &[u8]) -> Option<Packet<'_>> {
    if decoded.len() < OVERHEAD {
        return None;
    }
    let (body, crc) = decoded.split_at(decoded.len() - CRC_LEN);
    if crc32(body) != u32::from_be_bytes(crc.try_into().ok()?) {
        return None;
    }
    let session = u64::from_be_bytes(body[1..9].try_into().ok()?);
    let seq = u32::from_be_bytes(body[9..HEADER_LEN].try_into().ok()?);
    let payload = &body[HEADER_LEN..];
    match body[0] {
        KIND_DATA if !payload.is_empty() => Some(Packet::Data {
            session,
            seq,
            payload,
        }),
        KIND_ACK if payload.is_empty() => Some(Packet::Ack { session, seq }),
        _ => None,
    }
}
