//! gRPC development uploads. No operation is replayed after transmission.
use crate::grpc_codec::RawCodec;
use ota_protocol::{
    Request, RequestPayload as Op, Response, ResponsePayload, UploadAck, UploadPhase, VERSION,
};
use std::{fs, io, net::SocketAddr, path::Path, time::Duration};
use tokio::sync::mpsc;
use tonic::{client::Grpc, transport::Endpoint};

#[derive(Debug)]
pub struct UploadResult {
    pub bytes: u32,
    pub target_offset: u32,
    pub committed: bool,
}
fn invalid(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}
fn rpc(error: tonic::Status) -> io::Error {
    invalid(format!(
        "gRPC {}: {}",
        error.code(),
        error.message().escape_debug()
    ))
}
fn uncertain(error: io::Error) -> io::Error {
    invalid(format!(
        "commit outcome uncertain: {error}; reconnect and check firmware version before retrying"
    ))
}

pub fn upload(
    address: SocketAddr,
    timeout: Duration,
    path: &Path,
    version: &str,
    commit: bool,
) -> io::Result<UploadResult> {
    upload_impl(address, timeout, path, version, commit, None)
}
/// Consume management logs concurrently with the upload on the same connection.
/// Log failures are reported to the callback and never erase commit outcome.
pub fn upload_with_logs(
    address: SocketAddr,
    timeout: Duration,
    path: &Path,
    version: &str,
    commit: bool,
    on_log: &mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>),
) -> io::Result<UploadResult> {
    upload_impl(address, timeout, path, version, commit, Some(on_log))
}
fn upload_impl(
    address: SocketAddr,
    timeout: Duration,
    path: &Path,
    version: &str,
    commit: bool,
    on_log: Option<&mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>)>,
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // Each exchange applies the remaining absolute budget. Do not wrap this
            // whole future in a timeout that could erase commit-uncertainty context.
            run(address, timeout, &image, version, commit, on_log).await
        })
}
fn remaining(deadline: tokio::time::Instant, timeout: Duration) -> io::Result<Duration> {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .map(|v| v.min(timeout))
        .filter(|v| !v.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "upload total deadline exceeded"))
}
async fn run(
    address: SocketAddr,
    timeout: Duration,
    image: &[u8],
    version: &str,
    commit: bool,
    mut on_log: Option<&mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>)>,
) -> io::Result<UploadResult> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|e| invalid(e.to_string()))?
        .connect_timeout(timeout);
    let channel = tokio::time::timeout(remaining(deadline, timeout)?, endpoint.connect())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC connect timed out"))?
        .map_err(|e| invalid(format!("gRPC connect: {e}")))?;
    let mut client = Grpc::new(channel)
        .max_decoding_message_size(ota_protocol::MAX_FRAME)
        .max_encoding_message_size(ota_protocol::UPLOAD_FRAME);
    client.ready().await.map_err(|e| invalid(e.to_string()))?;
    let bytes = encode(
        Op::Status(ota_protocol::StatusRequest {
            include_boot: true,
            ..Default::default()
        }),
        1,
    )?;
    let reply = tokio::time::timeout(
        remaining(deadline, timeout)?,
        client.unary(
            tonic::Request::new(bytes),
            device_grpc::STATUS_PATH
                .parse()
                .map_err(|_| invalid("invalid status RPC path"))?,
            RawCodec,
        ),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC status timed out"))?
    .map_err(rpc)?;
    let response = decode(&reply.into_inner(), 1)?;
    let Some(ResponsePayload::Status(device)) = response.payload else {
        return Err(invalid("missing status"));
    };
    if !device.ota_available || device.application.as_str() != ota_core::APP {
        return Err(invalid(
            "device is not a provisioned development OTA target",
        ));
    }
    let running = device
        .boot()
        .and_then(|b| b.running())
        .ok_or_else(|| invalid("missing running partition"))?
        .offset;
    let expected_target = match running {
        0x10000 => 0x310000,
        0x310000 => 0x10000,
        _ => return Err(invalid("unexpected running slot")),
    };
    let mut log_stream = None;
    let mut cursor = ota_protocol::ReadLogsRequest::default();
    if let Some(callback) = on_log.as_mut() {
        let result = async {
            client.ready().await.map_err(|e| invalid(e.to_string()))?;
            let request = tonic::Request::new(encode(Op::ReadLogs(cursor.clone()), 77)?);
            let response = tokio::time::timeout(
                remaining(deadline, timeout)?,
                client.server_streaming(
                    request,
                    device_grpc::LOGS_PATH
                        .parse()
                        .map_err(|_| invalid("invalid logs RPC path"))?,
                    RawCodec,
                ),
            )
            .await
            .map_err(|_| invalid("log subscription timed out"))?
            .map_err(rpc)?;
            Ok::<_, io::Error>(response.into_inner())
        }
        .await;
        match result {
            Ok(stream) => log_stream = Some(stream),
            Err(e) => callback(Err(e.to_string())),
        }
    }
    let transfer = async {
        client.ready().await.map_err(|e| invalid(e.to_string()))?;
        let (tx, rx) = mpsc::channel(1);
        let mut request = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        request.set_timeout(remaining(deadline, Duration::from_secs(180))?);
        let response = tokio::time::timeout(
            remaining(deadline, timeout)?,
            client.streaming(
                request,
                device_grpc::UPLOAD_PATH
                    .parse()
                    .map_err(|_| invalid("invalid upload RPC path"))?,
                RawCodec,
            ),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC upload start timed out"))?
        .map_err(rpc)?;
        let mut replies = response.into_inner();
        let begin = ota_protocol::BeginUpdate {
            application: ota_core::APP
                .try_into()
                .map_err(|_| invalid("application name exceeds protocol capacity"))?,
            chip: "esp32s3"
                .try_into()
                .map_err(|_| invalid("chip name exceeds protocol capacity"))?,
            version: version
                .try_into()
                .map_err(|_| invalid("version too long"))?,
            image_size: image.len() as u32,
            sha256: ota_core::hash(image)
                .as_slice()
                .try_into()
                .map_err(|_| invalid("digest exceeds protocol capacity"))?,
        };
        let mut id = 1;
        let ack = exchange(&tx, &mut replies, id, Op::Begin(begin), deadline, timeout).await?;
        expect(&ack, UploadPhase::Receiving, 0, expected_target)?;
        for (i, bytes) in image.chunks(ota_protocol::CHUNK_SIZE).enumerate() {
            id += 1;
            let offset = (i * ota_protocol::CHUNK_SIZE) as u32;
            let ack = exchange(
                &tx,
                &mut replies,
                id,
                Op::Chunk(ota_protocol::FirmwareChunk {
                    offset,
                    data: bytes
                        .try_into()
                        .map_err(|_| invalid("chunk exceeds protocol capacity"))?,
                }),
                deadline,
                timeout,
            )
            .await?;
            expect(
                &ack,
                UploadPhase::Receiving,
                offset + bytes.len() as u32,
                expected_target,
            )?;
        }
        id += 1;
        let ack = exchange(
            &tx,
            &mut replies,
            id,
            Op::Finish(Default::default()),
            deadline,
            timeout,
        )
        .await?;
        expect(
            &ack,
            UploadPhase::Verified,
            image.len() as u32,
            expected_target,
        )?;
        id += 1;
        let final_op = if commit {
            Op::Commit(Default::default())
        } else {
            Op::Abort(Default::default())
        };
        let result = async {
            let ack = exchange(&tx, &mut replies, id, final_op, deadline, timeout).await?;
            expect(
                &ack,
                if commit {
                    UploadPhase::Committed
                } else {
                    UploadPhase::Aborted
                },
                image.len() as u32,
                expected_target,
            )
        }
        .await;
        if commit {
            result.map_err(uncertain)?;
        } else {
            result?;
        }
        // The validated Committed ACK is authoritative. Reset may truncate trailers.
        drop(tx);
        Ok(UploadResult {
            bytes: image.len() as u32,
            target_offset: expected_target,
            committed: commit,
        })
    };
    tokio::pin!(transfer);
    loop {
        if let Some(stream) = log_stream.as_mut() {
            tokio::select! {
                result=&mut transfer=>return result,
                message=stream.message()=> {
                    let result=match message {
                        Ok(Some(bytes))=>decode(&bytes,77).and_then(|response| match response.payload {
                            Some(ResponsePayload::Logs(batch))=>{validate_logs(&mut cursor,&batch)?;Ok(batch)},
                            _=>Err(invalid("missing log batch")),
                        }),
                        Ok(None)=>Err(invalid("log stream ended")),
                        Err(e)=>Err(rpc(e)),
                    };
                    let failed=result.is_err();
                    if let Some(callback)=on_log.as_mut(){callback(result.map_err(|e|e.to_string()));}
                    if failed {log_stream=None;}
                }
            }
        } else {
            return transfer.await;
        }
    }
}
fn encode(payload: Op, id: u32) -> io::Result<Vec<u8>> {
    ota_protocol::encode_upload(&Request {
        protocol_version: VERSION,
        request_id: id,
        payload: Some(payload),
    })
    .map(|b| b.to_vec())
    .map_err(|e| invalid(format!("encode upload: {e:?}")))
}
fn decode(bytes: &[u8], id: u32) -> io::Result<Response> {
    let response: Response =
        ota_protocol::decode(bytes).map_err(|_| invalid("invalid upload response"))?;
    if response.protocol_version != VERSION || response.request_id != id {
        return Err(invalid("upload version/request ID mismatch"));
    }
    Ok(response)
}
fn expect(ack: &UploadAck, phase: UploadPhase, offset: u32, target: u32) -> io::Result<()> {
    if ack.phase != phase || ack.next_offset != offset || ack.target_offset != target {
        Err(invalid("unexpected upload phase/offset/target"))
    } else {
        Ok(())
    }
}
async fn exchange(
    tx: &mpsc::Sender<Vec<u8>>,
    replies: &mut tonic::Streaming<Vec<u8>>,
    id: u32,
    payload: Op,
    deadline: tokio::time::Instant,
    timeout: Duration,
) -> io::Result<UploadAck> {
    let bytes = encode(payload, id)?;
    tokio::time::timeout(remaining(deadline, timeout)?, async {
        tx.send(bytes)
            .await
            .map_err(|_| invalid("request stream closed"))?;
        let bytes = replies
            .message()
            .await
            .map_err(rpc)?
            .ok_or_else(|| invalid("missing upload acknowledgement"))?;
        let response = decode(&bytes, id)?;
        match response.payload {
            Some(ResponsePayload::Upload(ack)) => Ok(ack),
            _ => Err(invalid("missing upload acknowledgement")),
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC upload operation timed out"))?
}

fn validate_logs(
    cursor: &mut ota_protocol::ReadLogsRequest,
    batch: &ota_protocol::ReadLogsResponse,
) -> io::Result<()> {
    if batch.boot_id == 0
        || (cursor.boot_id != 0 && cursor.boot_id != batch.boot_id && !batch.reset)
    {
        return Err(invalid("invalid log boot cursor"));
    }
    let mut previous = if batch.reset {
        0
    } else {
        cursor.after_sequence
    };
    for entry in &batch.entries {
        if entry.sequence <= previous {
            return Err(invalid("non-increasing log sequence"));
        }
        previous = entry.sequence;
    }
    if batch.next_sequence != previous {
        return Err(invalid("invalid log progress"));
    }
    cursor.boot_id = batch.boot_id;
    cursor.after_sequence = batch.next_sequence;
    Ok(())
}
