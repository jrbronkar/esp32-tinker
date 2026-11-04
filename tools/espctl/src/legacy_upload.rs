//! Unsigned development uploads. Never retries a request after transmission.
use ota_protocol::{
    Request, RequestPayload as Op, Response, ResponsePayload, UploadPhase, VERSION,
};
use std::{
    fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

use super::upload::UploadResult;
fn invalid(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

pub fn upload(
    address: SocketAddr,
    timeout: Duration,
    path: &Path,
    version: &str,
    commit: bool,
) -> io::Result<UploadResult> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > ota_core::SLOT_SIZE as u64 {
        return Err(invalid(
            "image must be an application binary smaller than 3 MiB",
        ));
    }
    let image = fs::read(path)?;
    ota_core::validate_image(
        &mut ota_core::SliceImage(&image),
        image.len() as u32,
        version,
    )
    .map_err(|e| {
        invalid(format!(
            "image validation: {e:?}; build device-demo and export an application-only image"
        ))
    })?;
    let device = super::query_status_with_options(address, timeout, false, true)?.status;
    if !device.ota_available || device.application.as_str() != ota_core::APP {
        return Err(invalid(
            "device is not a provisioned development OTA target",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut socket = loop {
        match TcpStream::connect_timeout(&address, super::remaining(deadline)?.min(timeout)) {
            Ok(s) => break s,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e),
        }
    };
    socket.set_nodelay(true)?;
    let digest = ota_core::hash(&image);
    let begin = ota_protocol::BeginUpdate {
        application: ota_core::APP.try_into().unwrap(),
        chip: "esp32s3".try_into().unwrap(),
        version: version
            .try_into()
            .map_err(|_| invalid("version too long"))?,
        image_size: image.len() as u32,
        sha256: digest.as_slice().try_into().unwrap(),
    };
    let mut id = 1;
    let ack = exchange(&mut socket, id, Op::Begin(begin), deadline, timeout)?;
    expect(&ack, UploadPhase::Receiving, 0)?;
    let target = ack.target_offset;
    if target != 0x10000 && target != 0x310000 {
        return Err(invalid("unexpected target slot"));
    }
    for (index, bytes) in image.chunks(ota_protocol::CHUNK_SIZE).enumerate() {
        id += 1;
        let offset = (index * ota_protocol::CHUNK_SIZE) as u32;
        let chunk = ota_protocol::FirmwareChunk {
            offset,
            data: bytes.try_into().unwrap(),
        };
        let ack = exchange(&mut socket, id, Op::Chunk(chunk), deadline, timeout)?;
        expect(&ack, UploadPhase::Receiving, offset + bytes.len() as u32)?;
        if ack.target_offset != target {
            return Err(invalid("target changed during transfer"));
        }
    }
    id += 1;
    let ack = exchange(
        &mut socket,
        id,
        Op::Finish(ota_protocol::FinishUpdate {}),
        deadline,
        timeout,
    )?;
    expect(&ack, UploadPhase::Verified, image.len() as u32)?;
    if ack.target_offset != target {
        return Err(invalid("target changed during verification"));
    }
    id += 1;
    if commit {
        let ack=exchange(&mut socket,id,Op::Commit(ota_protocol::CommitUpdate{}),deadline,timeout)
            .map_err(|e|invalid(format!("commit outcome uncertain: {e}; reconnect and check firmware version before retrying")))?;
        expect(&ack, UploadPhase::Committed, image.len() as u32)?;
    } else {
        let ack = exchange(
            &mut socket,
            id,
            Op::Abort(ota_protocol::AbortUpdate {}),
            deadline,
            timeout,
        )?;
        expect(&ack, UploadPhase::Aborted, image.len() as u32)?;
    }
    Ok(UploadResult {
        bytes: image.len() as u32,
        target_offset: target,
        committed: commit,
    })
}
fn expect(ack: &ota_protocol::UploadAck, phase: UploadPhase, offset: u32) -> io::Result<()> {
    if ack.phase != phase || ack.next_offset != offset {
        Err(invalid("unexpected upload phase/offset"))
    } else {
        Ok(())
    }
}
fn exchange(
    socket: &mut TcpStream,
    id: u32,
    payload: Op,
    deadline: Instant,
    timeout: Duration,
) -> io::Result<ota_protocol::UploadAck> {
    let request = Request {
        protocol_version: VERSION,
        request_id: id,
        payload: Some(payload),
    };
    let bytes = ota_protocol::encode_upload(&request)
        .map_err(|e| invalid(format!("encode upload: {e:?}")))?;
    write_exact(
        socket,
        &(bytes.len() as u32).to_be_bytes(),
        deadline,
        timeout,
    )?;
    write_exact(socket, &bytes, deadline, timeout)?;
    let mut prefix = [0; 4];
    read_exact(socket, &mut prefix, deadline, timeout)?;
    let n = ota_protocol::frame_length(prefix)
        .map_err(|_| invalid("invalid upload response length"))?;
    let mut body = [0; ota_protocol::MAX_FRAME];
    read_exact(socket, &mut body[..n], deadline, timeout)?;
    let response: Response =
        ota_protocol::decode(&body[..n]).map_err(|_| invalid("invalid upload response"))?;
    if response.protocol_version != VERSION || response.request_id != id {
        return Err(invalid("upload version/request ID mismatch"));
    }
    match response.payload {
        Some(ResponsePayload::Upload(ack)) => Ok(ack),
        Some(ResponsePayload::Error(error)) => Err(invalid(format!(
            "device rejected update: {}",
            error.message.escape_debug()
        ))),
        _ => Err(invalid("missing upload acknowledgement")),
    }
}

fn read_exact(
    socket: &mut TcpStream,
    mut bytes: &mut [u8],
    deadline: Instant,
    timeout: Duration,
) -> io::Result<()> {
    while !bytes.is_empty() {
        socket.set_read_timeout(Some(super::remaining(deadline)?.min(timeout)))?;
        let n = socket.read(bytes)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}
fn write_exact(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
    timeout: Duration,
) -> io::Result<()> {
    while !bytes.is_empty() {
        socket.set_write_timeout(Some(super::remaining(deadline)?.min(timeout)))?;
        let n = socket.write(bytes)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[n..];
    }
    Ok(())
}
