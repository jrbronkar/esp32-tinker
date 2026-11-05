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
        let mut handler = device_grpc::Management(|| {
            ota_protocol::status(
                "mock-esp32s3",
                "device-demo",
                "mock-v1",
                started.elapsed().as_millis() as u64,
            )
            .unwrap_or_default()
        });
        let _ = device_grpc::protolink::tokio::server(stream)
            .with_config(device_grpc::config())
            .build()
            .serve(&mut handler)
            .await;
    }
}
