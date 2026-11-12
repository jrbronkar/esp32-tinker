//! gRPC development uploads. No operation is replayed after transmission.
use crate::grpc_codec::RawCodec;
use ota_protocol::{UploadAck, UploadOperation as Op, UploadPhase, UploadRequest};
use std::{
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::Path,
    time::Duration,
};
use tokio::sync::mpsc;

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
    let image = PreparedImage::read(path, Some(version))?;
    transfer(
        address,
        timeout,
        &image,
        TransferPolicy {
            commit,
            ..Default::default()
        },
        on_log,
    )
}

pub(crate) struct PreparedImage {
    bytes: Vec<u8>,
    application: String,
    version: String,
}

impl PreparedImage {
    pub(crate) fn application(&self) -> &str {
        &self.application
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn read(path: &Path, version: Option<&str>) -> io::Result<Self> {
        if !fs::metadata(path)?.is_file() {
            return Err(invalid("image must be a regular application binary"));
        }
        let file = fs::File::open(path)?;
        let metadata = file.metadata()?;
        let limit = u64::from(ota_core::SLOT_SIZE);
        if !metadata.is_file() || metadata.len() > limit {
            return Err(invalid(
                "image must be an application binary no larger than 3 MiB",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(invalid("image exceeds the OTA slot size"));
        }
        let version = match version {
            Some(version) => version,
            None => ota_core::image_version(&bytes)
                .map_err(|error| invalid(format!("image header: {error:?}")))?,
        }
        .to_owned();
        let size = bytes.len() as u32;
        ota_core::validate_image(&mut ota_core::SliceImage(&bytes), size, &version).map_err(
            |e| {
                invalid(format!(
                    "image validation: {e:?}; export an OTA-capable application-only image"
                ))
            },
        )?;
        let application = ota_core::image_application(&bytes)
            .map_err(|error| invalid(format!("image application: {error:?}")))?
            .to_owned();
        Ok(Self {
            bytes,
            application,
            version,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Progress {
    Writing { written: u32, total: u32 },
    Verifying,
    Committing { target_offset: u32 },
    CheckingBoot,
}

#[derive(Default)]
struct TransferPolicy<'a> {
    commit: bool,
    device_id: Option<&'a str>,
    progress: Option<&'a mut dyn FnMut(Progress)>,
}

pub(crate) fn update_board(
    address: SocketAddr,
    timeout: Duration,
    image: &PreparedImage,
    device_id: &str,
    progress: &mut dyn FnMut(Progress),
    on_log: &mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>),
) -> io::Result<UploadResult> {
    transfer(
        address,
        timeout,
        image,
        TransferPolicy {
            commit: true,
            device_id: Some(device_id),
            progress: Some(progress),
        },
        Some(on_log),
    )
}

fn transfer(
    address: SocketAddr,
    timeout: Duration,
    image: &PreparedImage,
    policy: TransferPolicy<'_>,
    on_log: Option<&mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>)>,
) -> io::Result<UploadResult> {
    // Each exchange applies the remaining absolute budget, preserving
    // commit-uncertainty context if the connection fails during activation.
    crate::runtime()?.block_on(run(address, timeout, image, policy, on_log))
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
    image: &PreparedImage,
    mut policy: TransferPolicy<'_>,
    mut on_log: Option<&mut dyn FnMut(Result<ota_protocol::ReadLogsResponse, String>)>,
) -> io::Result<UploadResult> {
    let application = image.application.as_str();
    let version = image.version.as_str();
    let image = image.bytes.as_slice();
    let commit = policy.commit;
    let mut progress = |event| {
        if let Some(callback) = policy.progress.as_mut() {
            callback(event);
        }
    };
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(ota_protocol::MAX_UPLOAD_DURATION_SECS);
    let end = crate::deadline(std::time::Instant::now(), remaining(deadline, timeout)?)?;
    let mut client = crate::connect(
        address,
        end,
        &mut crate::QueryTimings::default(),
        on_log.is_some(),
    )
    .await?
    .max_encoding_message_size(ota_protocol::UPLOAD_FRAME);
    client.ready().await.map_err(|e| invalid(e.to_string()))?;
    let bytes = ota_protocol::encode(&ota_protocol::StatusRequest {
        include_boot: true,
        ..Default::default()
    })
    .map_err(|_| invalid("encode status request"))?
    .to_vec();
    let reply = tokio::time::timeout(
        remaining(deadline, timeout)?,
        client.unary(
            tonic::Request::new(bytes),
            ota_protocol::STATUS_PATH
                .parse()
                .map_err(|_| invalid("invalid status RPC path"))?,
            RawCodec,
        ),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC status timed out"))?
    .map_err(rpc)?;
    let device: ota_protocol::StatusResponse = ota_protocol::decode(&reply.into_inner())
        .map_err(|_| invalid("invalid status response"))?;
    if !device.ota_available {
        return Err(invalid(
            "device is not a provisioned development OTA target",
        ));
    }
    if policy
        .device_id
        .is_some_and(|id| device.device_id.as_str() != id)
    {
        return Err(invalid(
            "the board at this address no longer matches the selected device",
        ));
    }
    let running = device
        .boot()
        .and_then(|b| b.running())
        .ok_or_else(|| invalid("missing running partition"))?
        .offset;
    let expected_target = match running {
        ota_core::OTA_0_OFFSET => ota_core::OTA_1_OFFSET,
        ota_core::OTA_1_OFFSET => ota_core::OTA_0_OFFSET,
        _ => return Err(invalid("unexpected running slot")),
    };
    let mut log_stream = None;
    let mut cursor = ota_protocol::ReadLogsRequest::default();
    if let Some(callback) = on_log.as_mut() {
        let result = async {
            client.ready().await.map_err(|e| invalid(e.to_string()))?;
            let request = tonic::Request::new(
                ota_protocol::encode(&cursor)
                    .map_err(|_| invalid("encode log cursor"))?
                    .to_vec(),
            );
            let response = tokio::time::timeout(
                remaining(deadline, timeout)?,
                client.server_streaming(
                    request,
                    ota_protocol::LOGS_PATH
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
        request.set_timeout(remaining(
            deadline,
            Duration::from_secs(ota_protocol::MAX_UPLOAD_DURATION_SECS),
        )?);
        let response = tokio::time::timeout(
            remaining(deadline, timeout)?,
            client.streaming(
                request,
                ota_protocol::UPLOAD_PATH
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
            application: application
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
        let total = image.len() as u32;
        progress(Progress::Writing { written: 0, total });
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
            progress(Progress::Writing {
                written: offset + bytes.len() as u32,
                total,
            });
        }
        progress(Progress::Verifying);
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
            progress(Progress::Committing {
                target_offset: expected_target,
            });
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
                        Ok(Some(bytes))=>ota_protocol::decode::<ota_protocol::ReadLogsResponse>(&bytes)
                            .map_err(|_| invalid("invalid log batch")).and_then(|batch| {
                                super::validate_logs(&cursor,&batch)?;
                                cursor.boot_id=batch.boot_id;cursor.after_sequence=batch.next_sequence;Ok(batch)
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
    ota_protocol::encode_upload(&UploadRequest {
        request_id: id,
        payload: Some(payload),
    })
    .map(|b| b.to_vec())
    .map_err(|e| invalid(format!("encode upload: {e:?}")))
}
fn decode(bytes: &[u8], id: u32) -> io::Result<UploadAck> {
    let response: UploadAck =
        ota_protocol::decode(bytes).map_err(|_| invalid("invalid upload response"))?;
    if response.request_id != id {
        return Err(invalid("upload request ID mismatch"));
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
        Ok(response)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gRPC upload operation timed out"))?
}
