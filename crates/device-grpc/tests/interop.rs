use bytes::{Buf, BufMut};
use std::time::Duration;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

// Tonic supplies standard HTTP/2, gRPC framing and trailers; micropb supplies
// only protobuf bodies. This exercises independent transport implementations.
#[derive(Default)]
struct RawCodec;
struct RawEncoder;
struct RawDecoder;
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

#[tokio::test]
async fn tonic_status_repeated_calls_and_error_trailers() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut handler = device_grpc::Management(|| {
                ota_protocol::status("test-board", "host-test", "v1", 42).unwrap()
            });
            protolink::tokio::server(stream)
                .with_config(device_grpc::config())
                .build()
                .serve(&mut handler)
                .await
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = tonic::client::Grpc::new(channel);
        for id in 1..=8 {
            client.ready().await.unwrap();
            let request = ota_protocol::encode(&ota_protocol::status_request(id)).unwrap();
            let reply = client
                .unary(
                    tonic::Request::new(request.to_vec()),
                    device_grpc::STATUS_PATH.parse().unwrap(),
                    RawCodec,
                )
                .await
                .unwrap();
            let reply: ota_protocol::Response = ota_protocol::decode(&reply.into_inner()).unwrap();
            assert_eq!(reply.request_id, id);
            let Some(ota_protocol::ResponsePayload::Status(status)) = reply.payload else {
                panic!("missing status")
            };
            assert_eq!(status.device_id, "test-board");
            assert_eq!(status.uptime_ms, 42);
        }
        for (path, body, code) in [
            (
                device_grpc::STATUS_PATH,
                vec![0xff],
                tonic::Code::InvalidArgument,
            ),
            (
                "/unknown.Service/Method",
                vec![],
                tonic::Code::Unimplemented,
            ),
        ] {
            client.ready().await.unwrap();
            let error = client
                .unary(tonic::Request::new(body), path.parse().unwrap(), RawCodec)
                .await
                .unwrap_err();
            assert_eq!(error.code(), code);
        }
        server.abort();
        let _ = server.await;
    })
    .await
    .expect("interop timed out");
}
