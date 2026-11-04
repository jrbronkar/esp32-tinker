use crate::{
    flash_target::FlashTarget,
    status::{read_exact, write_all},
};
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration};
use ota_protocol::{
    Request, RequestPayload as Op, Response, ResponsePayload, UploadAck, UploadPhase,
    SESSION_TIMEOUT_SECS, VERSION,
};

pub async fn session(
    socket: &mut TcpSocket<'_>,
    first: Request,
    target: &mut FlashTarget,
    runtime: &crate::runtime::Runtime,
) -> Result<(), ()> {
    let _guard = crate::runtime::UploadGuard::new(runtime);
    let mut last_id = first.request_id;
    let result = transfer(socket, first, target, &mut last_id, runtime).await;
    if let Err(error) = result {
        // Best effort error, then abandon all in-memory transfer state.
        let message: heapless::String<64> = match error {
            ota_core::Error::State => "invalid upload state",
            ota_core::Error::Metadata => "incompatible metadata",
            ota_core::Error::Bounds => "image or chunk outside limits",
            ota_core::Error::Order => "unexpected chunk offset",
            ota_core::Error::Storage => "flash operation failed",
            ota_core::Error::Hash => "stored image digest mismatch",
            ota_core::Error::Image => "invalid ESP application image",
        }
        .try_into()
        .unwrap();
        let response = Response {
            protocol_version: VERSION,
            request_id: last_id,
            payload: Some(ResponsePayload::Error(ota_protocol::ErrorResponse {
                code: ota_protocol::ErrorCode::InvalidRequest,
                message,
            })),
        };
        let _ = send(socket, &response).await;
        return Err(());
    }
    Ok(())
}
async fn transfer(
    socket: &mut TcpSocket<'_>,
    first: Request,
    target: &mut FlashTarget,
    last_id: &mut u32,
    runtime: &crate::runtime::Runtime,
) -> Result<(), ota_core::Error> {
    use ota_core::Error;
    if first.protocol_version != VERSION {
        return Err(Error::Metadata);
    }
    let Op::Begin(begin) = first.payload.ok_or(Error::State)? else {
        return Err(Error::State);
    };
    let mut update = ota_core::Update::begin(
        target,
        &begin.application,
        &begin.chip,
        &begin.version,
        begin.image_size,
        &begin.sha256,
    )?;
    runtime.transition(crate::runtime::State::Receiving);
    let mut id = first.request_id;
    ack(socket, id, UploadPhase::Receiving, 0, target.offset).await?;
    loop {
        let mut prefix = [0; 4];
        with_timeout(
            Duration::from_secs(SESSION_TIMEOUT_SECS),
            read_exact(socket, &mut prefix),
        )
        .await
        .map_err(|_| Error::State)?
        .map_err(|_| Error::State)?;
        let n = ota_protocol::upload_frame_length(prefix).map_err(|_| Error::Bounds)?;
        let mut body = [0; ota_protocol::UPLOAD_FRAME];
        with_timeout(
            Duration::from_secs(SESSION_TIMEOUT_SECS),
            read_exact(socket, &mut body[..n]),
        )
        .await
        .map_err(|_| Error::State)?
        .map_err(|_| Error::State)?;
        let request = ota_protocol::decode_upload(&body[..n]).map_err(|_| Error::Metadata)?;
        if request.protocol_version != VERSION
            || request.request_id != id.checked_add(1).ok_or(Error::State)?
        {
            return Err(Error::Order);
        }
        id = request.request_id;
        *last_id = id;
        match request.payload.ok_or(Error::State)? {
            Op::Chunk(chunk) => {
                update.chunk(target, chunk.offset, &chunk.data)?;
                ack(
                    socket,
                    id,
                    UploadPhase::Receiving,
                    update.received(),
                    target.offset,
                )
                .await?;
            }
            Op::Finish(_) => {
                update.finish(target)?;
                runtime.transition(crate::runtime::State::Verified);
                ack(
                    socket,
                    id,
                    UploadPhase::Verified,
                    update.received(),
                    target.offset,
                )
                .await?;
            }
            Op::Abort(_) => {
                ack(
                    socket,
                    id,
                    UploadPhase::Aborted,
                    update.received(),
                    target.offset,
                )
                .await?;
                return Ok(());
            }
            Op::Commit(_) => {
                runtime.transition(crate::runtime::State::Committing);
                update.commit(target)?;
                // Boot selection has changed even if the response is lost.
                let _ = ack(
                    socket,
                    id,
                    UploadPhase::Committed,
                    update.received(),
                    target.offset,
                )
                .await;
                esp_hal::system::software_reset();
            }
            _ => return Err(Error::State),
        }
    }
}
async fn ack(
    socket: &mut TcpSocket<'_>,
    id: u32,
    phase: UploadPhase,
    next_offset: u32,
    target_offset: u32,
) -> Result<(), ota_core::Error> {
    send(
        socket,
        &Response {
            protocol_version: VERSION,
            request_id: id,
            payload: Some(ResponsePayload::Upload(UploadAck {
                phase,
                next_offset,
                target_offset,
            })),
        },
    )
    .await
    .map_err(|_| ota_core::Error::State)
}
async fn send(socket: &mut TcpSocket<'_>, response: &Response) -> Result<(), ()> {
    let bytes = ota_protocol::encode(response).map_err(|_| ())?;
    write_all(socket, &(bytes.len() as u32).to_be_bytes()).await?;
    write_all(socket, &bytes).await?;
    with_timeout(Duration::from_secs(SESSION_TIMEOUT_SECS), socket.flush())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}
