#![allow(dead_code)]
use bytes::{Buf, BufMut};
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

// Tonic supplies standard HTTP/2, gRPC framing and trailers; micropb supplies
// only protobuf bodies. This exercises independent transport implementations.
#[derive(Default)]
pub struct RawCodec;
pub struct RawEncoder;
pub struct RawDecoder;
impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;
    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}
impl Encoder for RawEncoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;
    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        dst.put_slice(&item);
        Ok(())
    }
}
impl Decoder for RawDecoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(Some(src.copy_to_bytes(src.remaining()).to_vec()))
    }
}

pub type Client = tonic::client::Grpc<tonic::transport::Channel>;
pub fn body(payload: ota_protocol::UploadOperation, id: u32) -> Vec<u8> {
    ota_protocol::encode_upload(&ota_protocol::UploadRequest {
        request_id: id,
        payload: Some(payload),
    })
    .unwrap()
    .to_vec()
}
pub fn response(bytes: &[u8]) -> ota_protocol::UploadAck {
    ota_protocol::decode(bytes).unwrap()
}
pub async fn logs(
    client: &mut Client,
    cursor: ota_protocol::ReadLogsRequest,
) -> Result<tonic::Streaming<Vec<u8>>, tonic::Status> {
    client
        .ready()
        .await
        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
    Ok(client
        .server_streaming(
            tonic::Request::new(proto(&cursor)),
            device_grpc::LOGS_PATH.parse().unwrap(),
            RawCodec,
        )
        .await?
        .into_inner())
}
pub async fn upload(
    client: &mut Client,
) -> Result<
    (
        tokio::sync::mpsc::Sender<Vec<u8>>,
        tonic::Streaming<Vec<u8>>,
    ),
    tonic::Status,
> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    client
        .ready()
        .await
        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
    let response = client
        .streaming(
            tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx)),
            device_grpc::UPLOAD_PATH.parse().unwrap(),
            RawCodec,
        )
        .await?;
    Ok((tx, response.into_inner()))
}
pub async fn op(
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    stream: &mut tonic::Streaming<Vec<u8>>,
    payload: ota_protocol::UploadOperation,
    id: u32,
) -> Result<ota_protocol::UploadAck, tonic::Status> {
    tx.send(body(payload, id))
        .await
        .map_err(|_| tonic::Status::unavailable("request stream closed"))?;
    let bytes = stream
        .message()
        .await?
        .ok_or_else(|| tonic::Status::internal("missing ack"))?;
    let response = response(&bytes);
    if response.request_id != id {
        return Err(tonic::Status::internal("wrong ack request id"));
    }
    Ok(response)
}
pub fn begin(image: &[u8], version: &str) -> ota_protocol::UploadOperation {
    ota_protocol::UploadOperation::Begin(ota_protocol::BeginUpdate {
        application: "device-demo".try_into().unwrap(),
        chip: "esp32s3".try_into().unwrap(),
        version: version.try_into().unwrap(),
        image_size: image.len() as u32,
        sha256: ota_core::hash(image).as_slice().try_into().unwrap(),
    })
}
pub fn chunk(offset: usize, bytes: &[u8]) -> ota_protocol::UploadOperation {
    ota_protocol::UploadOperation::Chunk(ota_protocol::FirmwareChunk {
        offset: offset as u32,
        data: bytes.try_into().unwrap(),
    })
}

pub fn proto(message: &impl ota_protocol::MessageEncode) -> Vec<u8> {
    ota_protocol::encode(message).unwrap().to_vec()
}
pub fn log_response(bytes: &[u8]) -> ota_protocol::ReadLogsResponse {
    ota_protocol::decode(bytes).unwrap()
}
pub fn status_response(bytes: &[u8]) -> ota_protocol::StatusResponse {
    ota_protocol::decode(bytes).unwrap()
}
