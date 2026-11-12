#![no_std]

#[allow(nonstandard_style, unused, irrefutable_let_patterns, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/ota.rs"));
}
// Expose the package module for the generated server bindings.
#[doc(hidden)]
pub use generated::ota_;
pub use generated::ota_::{
    AbortUpdate, BeginUpdate, BootInfo, CommitUpdate, DeviceTimings, FinishUpdate, FirmwareChunk,
    LogEntry, LogEntry_::Severity as LogSeverity, PartitionInfo, ReadLogsRequest, ReadLogsResponse,
    StatusRequest, StatusResponse, UploadAck, UploadAck_::Phase as UploadPhase, UploadRequest,
    UploadRequest_::Payload as UploadOperation,
};
include!(concat!(env!("OUT_DIR"), "/rpc_paths.rs"));
use micropb::PbEncoder;
pub use micropb::{MessageDecode, MessageEncode};

pub const PORT: u16 = 7779;
pub const SERVICE_TYPE: &str = "_espctl._tcp.local.";

/// DNS-SD TXT keys shared by the advertiser and discovery client.
pub mod txt {
    pub const ID: &str = "id";
    pub const APPLICATION: &str = "application";
    pub const FIRMWARE: &str = "firmware";
    pub const STATE: &str = "state";
    pub const OTA: &str = "ota";
    pub const TRANSPORT: &str = "transport";
}

pub const MAX_FRAME: usize = 512;
pub const SESSION_TIMEOUT_SECS: u64 = 5;
pub const MAX_UPLOAD_DURATION_SECS: u64 = 180;
pub type Frame = heapless::Vec<u8, MAX_FRAME>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidLength,
    Encode,
    Decode,
    FieldTooLong,
}

/// Encode a bounded protobuf body; gRPC supplies transport framing.
pub fn encode<M: MessageEncode>(message: &M) -> Result<Frame, Error> {
    let mut encoder = PbEncoder::new(Frame::new());
    message.encode(&mut encoder).map_err(|_| Error::Encode)?;
    Ok(encoder.into_writer())
}

pub fn decode<M: MessageDecode + Default>(bytes: &[u8]) -> Result<M, Error> {
    if bytes.len() > MAX_FRAME {
        return Err(Error::InvalidLength);
    }
    let mut message = M::default();
    message
        .decode_from_bytes(bytes)
        .map_err(|_| Error::Decode)?;
    Ok(message)
}

/// Fixed-capacity status data shared by firmware and host mocks.
/// Identity is descriptive; gRPC does not authenticate either endpoint.
pub fn status(
    device_id: &str,
    application: &str,
    firmware_version: &str,
    uptime_ms: u64,
) -> Result<StatusResponse, Error> {
    Ok(StatusResponse {
        device_id: device_id.try_into().map_err(|_| Error::FieldTooLong)?,
        chip: "esp32s3".try_into().map_err(|_| Error::FieldTooLong)?,
        application: application.try_into().map_err(|_| Error::FieldTooLong)?,
        firmware_version: firmware_version
            .try_into()
            .map_err(|_| Error::FieldTooLong)?,
        uptime_ms,
        ota_available: false,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_request_uses_its_own_wire_format() {
        let request = StatusRequest {
            include_boot: true,
            include_timings: true,
        };
        assert_eq!(encode(&request).unwrap().as_slice(), &[8, 1, 16, 1]);
        assert_eq!(
            decode::<StatusRequest>(&[8, 1, 16, 1, 0xf8, 1, 7]).unwrap(),
            request
        );
    }
    #[test]
    fn response_roundtrip_and_u64_uptime() {
        let reply = status("board", "soil", "1", u64::MAX).unwrap();
        assert_eq!(
            decode::<StatusResponse>(&encode(&reply).unwrap()).unwrap(),
            reply
        );
    }
    #[test]
    fn bounded_boot_inventory_and_timings_fit_a_control_frame() {
        let text = core::str::from_utf8(&[b'x'; 64]).unwrap();
        let mut current = status(text, text, text, u64::MAX).unwrap();
        current.management_state = "abcdefghijklmnop".try_into().unwrap();
        current.set_timings(DeviceTimings {
            decode_us: u64::MAX,
            prepare_us: u64::MAX,
        });
        let part = PartitionInfo {
            label: "abcdefghijklmnop".try_into().unwrap(),
            offset: u32::MAX,
            size: u32::MAX,
        };
        let mut boot = BootInfo {
            configured_flash_size_bytes: u32::MAX,
            partition_count: u32::MAX,
            ota_app_partition_count: u32::MAX,
            has_ota_data: true,
            layout_valid: true,
            error: text.try_into().unwrap(),
            ..Default::default()
        };
        boot.set_running(part.clone());
        boot.set_ota_0(part.clone());
        boot.set_ota_1(part);
        current.set_boot(boot);
        let response = current;
        let bytes = encode(&response).unwrap();
        assert!(bytes.len() <= MAX_FRAME);
        assert_eq!(decode::<StatusResponse>(&bytes).unwrap(), response);
    }

    #[test]
    fn oversized_and_malformed_protobuf_are_rejected() {
        assert_eq!(
            decode::<StatusRequest>(&[0; MAX_FRAME + 1]),
            Err(Error::InvalidLength)
        );
        assert!(decode::<UploadRequest>(&[0x08, 0x80]).is_err());
        assert!(decode::<UploadAck>(&[0xff]).is_err());
    }

    #[test]
    fn field_capacity_is_enforced_on_decode() {
        // StatusResponse.device_id: length-delimited string of 65 bytes.
        let mut bytes = heapless::Vec::<u8, 80>::new();
        bytes.extend_from_slice(&[0x0a, 65]).unwrap();
        bytes.extend_from_slice(&[b'x'; 65]).unwrap();
        assert!(decode::<StatusResponse>(&bytes).is_err());
        assert_eq!(
            status(core::str::from_utf8(&[b'x'; 65]).unwrap(), "soil", "1", 0),
            Err(Error::FieldTooLong)
        );
    }

    #[test]
    fn default_rpc_requests_are_valid_empty_protobuf_messages() {
        assert!(encode(&StatusRequest::default()).unwrap().is_empty());
        assert_eq!(
            decode::<StatusRequest>(&[]).unwrap(),
            StatusRequest::default()
        );
        assert_eq!(
            decode::<ReadLogsRequest>(&[]).unwrap(),
            ReadLogsRequest::default()
        );
    }
}

/// Upload requests can contain a full flash sector.
pub const UPLOAD_FRAME: usize = 4608;
pub const CHUNK_SIZE: usize = 4096;
pub fn encode_upload<M: MessageEncode>(
    message: &M,
) -> Result<heapless::Vec<u8, UPLOAD_FRAME>, Error> {
    let mut encoder = PbEncoder::new(heapless::Vec::<u8, UPLOAD_FRAME>::new());
    message.encode(&mut encoder).map_err(|_| Error::Encode)?;
    Ok(encoder.into_writer())
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    #[test]
    fn upload_operations_roundtrip() {
        let request = UploadRequest {
            request_id: 7,
            payload: Some(UploadOperation::Begin(BeginUpdate {
                application: "device-demo".try_into().unwrap(),
                chip: "esp32s3".try_into().unwrap(),
                version: "release".try_into().unwrap(),
                image_size: 4096,
                sha256: (&[0u8; 32][..]).try_into().unwrap(),
            })),
        };
        let bytes = encode(&request).unwrap();
        assert_eq!(decode::<UploadRequest>(&bytes).unwrap(), request);
        assert_eq!(&bytes[..3], &[8, 7, 18]);
        let ack = UploadAck {
            request_id: 7,
            phase: UploadPhase::Receiving,
            ..Default::default()
        };
        let bytes = encode(&ack).unwrap();
        assert_eq!(bytes.as_slice(), &[8, 7, 16, 1]);
        assert_eq!(decode::<UploadAck>(&bytes).unwrap(), ack);
    }

    #[test]
    fn sector_chunk_requires_upload_frame_limit() {
        let chunk = FirmwareChunk {
            offset: 4096,
            data: (&[7u8; 4096][..]).try_into().unwrap(),
        };
        let request = UploadRequest {
            request_id: 2,
            payload: Some(UploadOperation::Chunk(chunk)),
        };
        let bytes = encode_upload(&request).unwrap();
        assert!(bytes.len() <= UPLOAD_FRAME);
        assert_eq!(decode::<UploadRequest>(&bytes), Err(Error::InvalidLength));
        let mut decoded = UploadRequest::default();
        decoded.decode_from_bytes(&bytes).unwrap();
        assert_eq!(decoded, request);
    }
}

pub mod logs;
