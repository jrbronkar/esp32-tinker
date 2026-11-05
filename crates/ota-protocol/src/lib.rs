#![no_std]

#[allow(nonstandard_style, unused, irrefutable_let_patterns, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/ota.rs"));
}
pub use generated::ota_::{
    AbortUpdate, BeginUpdate, BootInfo, CommitUpdate, DeviceTimings, ErrorResponse,
    ErrorResponse_::Code as ErrorCode, FinishUpdate, FirmwareChunk, LogEntry,
    LogEntry_::Severity as LogSeverity, PartitionInfo, ReadLogsRequest, ReadLogsResponse, Request,
    Request_::Payload as RequestPayload, Response, Response_::Payload as ResponsePayload,
    StatusRequest, StatusResponse, UploadAck, UploadAck_::Phase as UploadPhase,
};
use micropb::{MessageDecode, MessageEncode, PbEncoder};

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
pub type Frame = heapless::Vec<u8, MAX_FRAME>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidLength,
    Encode,
    Decode,
    FieldTooLong,
}

pub fn frame_length(prefix: [u8; 4]) -> Result<usize, Error> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 || len > MAX_FRAME {
        Err(Error::InvalidLength)
    } else {
        Ok(len)
    }
}

/// Encode a bounded protobuf body; gRPC supplies transport framing.
pub fn encode<M: MessageEncode>(message: &M) -> Result<Frame, Error> {
    let mut encoder = PbEncoder::new(Frame::new());
    message.encode(&mut encoder).map_err(|_| Error::Encode)?;
    let bytes = encoder.into_writer();
    frame_length((bytes.len() as u32).to_be_bytes())?;
    Ok(bytes)
}

pub fn decode<M: MessageDecode + Default>(bytes: &[u8]) -> Result<M, Error> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(Error::InvalidLength);
    }
    let mut message = M::default();
    message
        .decode_from_bytes(bytes)
        .map_err(|_| Error::Decode)?;
    Ok(message)
}

pub fn status_request(request_id: u32) -> Request {
    Request {
        request_id,
        payload: Some(RequestPayload::Status(StatusRequest::default())),
    }
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

pub fn respond(request: Request, status: StatusResponse) -> Response {
    let payload = if !matches!(request.payload, Some(RequestPayload::Status(_))) {
        error_payload(
            ErrorCode::InvalidRequest,
            "missing or unsupported operation",
        )
    } else {
        ResponsePayload::Status(status)
    };
    Response {
        request_id: request.request_id,
        payload: Some(payload),
    }
}

fn error_payload(code: ErrorCode, message: &str) -> ResponsePayload {
    ResponsePayload::Error(ErrorResponse {
        code,
        message: message.try_into().expect("static error fits 64 bytes"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_matches_protobuf_wire_format() {
        let bytes = encode(&status_request(1)).unwrap();
        assert_eq!(bytes.as_slice(), &[0x10, 1, 0x52, 0]);
        assert_eq!(decode::<Request>(&bytes).unwrap(), status_request(1));
        // Older senders' reserved field and future unknown fields are ignored.
        assert_eq!(
            decode::<Request>(&[0x08, 1, 0x10, 1, 0x52, 0, 0xf8, 0x01, 7]).unwrap(),
            status_request(1)
        );
    }

    #[test]
    fn response_roundtrip_and_u64_uptime() {
        let reply = respond(status_request(1), status("board", "soil", "1", 0).unwrap());
        let mut bytes = encode(&reply).unwrap();
        bytes.extend_from_slice(&[0x08, 1, 0xf8, 0x01, 7]).unwrap();
        assert_eq!(decode::<Response>(&bytes).unwrap(), reply);
        let reply = respond(
            status_request(42),
            status("board", "soil-demo", "v1", u64::MAX).unwrap(),
        );
        assert_eq!(decode::<Response>(&encode(&reply).unwrap()).unwrap(), reply);
    }

    #[test]
    fn bounded_boot_inventory_and_timings_fit_a_control_frame() {
        let text = core::str::from_utf8(&[b'x'; 64]).unwrap();
        let mut current = status(text, text, text, u64::MAX).unwrap();
        current.management_state = "abcdefghijklmnop".try_into().unwrap();
        current.set_timings(DeviceTimings {
            request_read_us: u64::MAX,
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
        let response = respond(status_request(u32::MAX), current);
        let bytes = encode(&response).unwrap();
        assert!(bytes.len() <= MAX_FRAME);
        assert_eq!(decode::<Response>(&bytes).unwrap(), response);
    }

    #[test]
    fn invalid_frames_and_malformed_protobuf() {
        assert_eq!(frame_length(0u32.to_be_bytes()), Err(Error::InvalidLength));
        assert_eq!(
            frame_length(513u32.to_be_bytes()),
            Err(Error::InvalidLength)
        );
        assert_eq!(
            frame_length(u32::MAX.to_be_bytes()),
            Err(Error::InvalidLength)
        );
        assert_eq!(frame_length(512u32.to_be_bytes()), Ok(512));
        assert!(decode::<Request>(&[0x08, 0x80]).is_err());
        assert!(decode::<Response>(&[0xff]).is_err());
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
    fn semantic_errors_echo_request_id() {
        let request = Request {
            payload: None,
            ..status_request(7)
        };
        let reply = respond(request, status("board", "soil", "1", 0).unwrap());
        assert_eq!(reply.request_id, 7);
        assert!(
            matches!(reply.payload, Some(ResponsePayload::Error(e)) if e.code == ErrorCode::InvalidRequest)
        );
    }
}

/// Upload sessions have separate bounded frames; initial/control frames stay at 512 bytes.
pub const UPLOAD_FRAME: usize = 4608;
pub const CHUNK_SIZE: usize = 4096;
pub fn upload_frame_length(prefix: [u8; 4]) -> Result<usize, Error> {
    let n = u32::from_be_bytes(prefix) as usize;
    if n == 0 || n > UPLOAD_FRAME {
        Err(Error::InvalidLength)
    } else {
        Ok(n)
    }
}
pub fn encode_upload<M: MessageEncode>(
    message: &M,
) -> Result<heapless::Vec<u8, UPLOAD_FRAME>, Error> {
    let mut encoder = PbEncoder::new(heapless::Vec::<u8, UPLOAD_FRAME>::new());
    message.encode(&mut encoder).map_err(|_| Error::Encode)?;
    let bytes = encoder.into_writer();
    upload_frame_length((bytes.len() as u32).to_be_bytes())?;
    Ok(bytes)
}
pub fn decode_upload(bytes: &[u8]) -> Result<Request, Error> {
    upload_frame_length((bytes.len() as u32).to_be_bytes())?;
    let mut request = Request::default();
    request
        .decode_from_bytes(bytes)
        .map_err(|_| Error::Decode)?;
    Ok(request)
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    #[test]
    fn upload_and_status_share_envelope_without_enabling_upload_in_status_handler() {
        let request = Request {
            request_id: 7,
            payload: Some(RequestPayload::Begin(BeginUpdate {
                application: "device-demo".try_into().unwrap(),
                chip: "esp32s3".try_into().unwrap(),
                version: "release".try_into().unwrap(),
                image_size: 4096,
                sha256: (&[0u8; 32][..]).try_into().unwrap(),
            })),
        };
        let bytes = encode(&request).unwrap();
        assert_eq!(decode::<Request>(&bytes).unwrap(), request);
        let response = respond(
            request,
            status("board", "device-demo", "release", 0).unwrap(),
        );
        assert!(
            matches!(response.payload, Some(ResponsePayload::Error(e)) if e.code == ErrorCode::InvalidRequest)
        );
    }

    #[test]
    fn sector_chunk_requires_upload_frame_limit() {
        let chunk = FirmwareChunk {
            offset: 4096,
            data: (&[7u8; 4096][..]).try_into().unwrap(),
        };
        let request = Request {
            request_id: 2,
            payload: Some(RequestPayload::Chunk(chunk)),
        };
        let bytes = encode_upload(&request).unwrap();
        assert!(bytes.len() <= UPLOAD_FRAME);
        assert!(frame_length((bytes.len() as u32).to_be_bytes()).is_err());
        assert_eq!(decode_upload(&bytes).unwrap(), request);
        assert!(upload_frame_length(4609u32.to_be_bytes()).is_err());
    }
}

pub mod logs;
