#[path = "support/grpc.rs"]
mod common;
use common::*;
use std::time::Duration;
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
