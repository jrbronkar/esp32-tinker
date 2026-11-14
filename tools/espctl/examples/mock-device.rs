//! Host-only gRPC stand-in; does not emulate radio or flash.
use std::time::Instant;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("127.0.0.1:{}", device_grpc::PORT));
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("MOCK gRPC device listening on {}", listener.local_addr()?);
    let started = Instant::now();
    loop {
        let (stream, _) = listener.accept().await?;
        let handler = management_grpc::ManagementServer::new(Mock(started));
        let _ = embedded_grpc::serve_streaming(
            embedded_io_adapters::tokio_1::FromTokio::new(stream),
            &handler,
        )
        .await;
    }
}

struct Mock(Instant);

impl management_grpc::Management for Mock {
    async fn status(
        &self,
        _: ota_protocol::StatusRequest,
    ) -> Result<ota_protocol::StatusResponse, embedded_grpc::Status> {
        Ok(ota_protocol::status(
            "mock-esp32s3",
            "device-demo",
            "mock-v1",
            self.0.elapsed().as_millis() as u64,
        )
        .unwrap_or_default())
    }

    async fn logs(
        &self,
        _: ota_protocol::ReadLogsRequest,
    ) -> Result<
        impl embedded_grpc::Stream<Item = Result<ota_protocol::ReadLogsResponse, embedded_grpc::Status>>,
        embedded_grpc::Status,
    > {
        Err::<tokio_stream::Empty<_>, _>(embedded_grpc::Status::Unimplemented)
    }

    async fn upload(
        &self,
        _: embedded_grpc::Request<
            embedded_grpc::Streaming<ota_protocol::UploadRequest, { ota_protocol::UPLOAD_FRAME }>,
        >,
    ) -> Result<
        embedded_grpc::Response<
            impl embedded_grpc::Stream<Item = Result<ota_protocol::UploadAck, embedded_grpc::Status>>,
        >,
        embedded_grpc::Status,
    > {
        Err::<embedded_grpc::Response<tokio_stream::Empty<_>>, _>(
            embedded_grpc::Status::Unimplemented,
        )
    }
}
