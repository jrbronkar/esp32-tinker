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
            let mut handler = device_grpc::ManagementServer::new(device_grpc::Management(|| {
                ota_protocol::status("test-board", "host-test", "v1", 42).unwrap()
            }));
            protolink::tokio::server(stream)
                .with_config(device_grpc::config())
                .build()
                .serve(&mut handler)
                .await
        });
        check_status_exchanges(addr).await;
        server.abort();
        let _ = server.await;
    })
    .await
    .expect("interop timed out");
}

async fn check_status_exchanges(addr: std::net::SocketAddr) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = tonic::client::Grpc::new(channel);
    for _ in 1..=8 {
        client.ready().await.unwrap();
        let request = ota_protocol::encode(&ota_protocol::StatusRequest::default()).unwrap();
        let reply = client
            .unary(
                tonic::Request::new(request.to_vec()),
                ota_protocol::STATUS_PATH.parse().unwrap(),
                RawCodec,
            )
            .await
            .unwrap();
        let status = status_response(&reply.into_inner());
        assert_eq!(status.device_id, "test-board");
        assert_eq!(status.uptime_ms, 42);
    }
    for (path, body, code) in [
        (
            ota_protocol::STATUS_PATH,
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
}

struct EmbeddedManagement;

impl management_grpc::Management for EmbeddedManagement {
    async fn status(
        &mut self,
        request: ota_protocol::StatusRequest,
    ) -> Result<ota_protocol::StatusResponse, management_grpc::Status> {
        if request.include_timings {
            return Err(management_grpc::Status::Unavailable);
        }
        let mut reply = ota_protocol::status("test-board", "host-test", "v1", 42).unwrap();
        if request.include_boot {
            reply.set_boot(ota_protocol::BootInfo {
                configured_flash_size_bytes: 16 * 1024 * 1024,
                ..Default::default()
            });
        }
        Ok(reply)
    }
}

#[tokio::test]
async fn embedded_unary_status_with_tonic_and_espctl() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::task::spawn_local(async move {
                    for _ in 0..3 {
                        let (socket, _) = listener.accept().await.unwrap();
                        let error = embedded_grpc::serve(
                            embedded_io_adapters::tokio_1::FromTokio::new(socket),
                            &mut management_grpc::ManagementServer::new(EmbeddedManagement),
                        )
                        .await
                        .unwrap_err();
                        assert!(error.is_disconnected(), "{error:?}");
                    }
                });

                check_status_exchanges(addr).await;

                let mut client = tonic::client::Grpc::new(
                    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                        .unwrap()
                        .connect()
                        .await
                        .unwrap(),
                );
                client.ready().await.unwrap();
                let request = ota_protocol::StatusRequest {
                    include_timings: true,
                    ..Default::default()
                };
                let error = client
                    .unary(
                        tonic::Request::new(proto(&request)),
                        ota_protocol::STATUS_PATH.parse().unwrap(),
                        RawCodec,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.code(), tonic::Code::Unavailable);
                drop(client);

                let status = tokio::task::spawn_blocking(move || {
                    espctl::query_status_with_options(addr, Duration::from_secs(3), false, true)
                })
                .await
                .unwrap()
                .unwrap()
                .status;
                assert_eq!(status.device_id, "test-board");
                assert_eq!(status.boot.configured_flash_size_bytes, 16 * 1024 * 1024);
                server.await.unwrap();
            })
            .await
            .expect("embedded interop timed out");
        })
        .await;
}
