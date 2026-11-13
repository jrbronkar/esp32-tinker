use std::{
    io::{self, Write},
    net::Ipv4Addr,
};

use embedded_http2::{
    HeaderField,
    service::{Request, Response, Service, serve},
};
use embedded_io_adapters::tokio_1::FromTokio;
use http::StatusCode;
use tokio::net::TcpListener;

struct Fixture;

impl Service for Fixture {
    async fn call(&mut self, _request: Request) -> Response {
        Response {
            headers: vec![HeaderField {
                name: ":status".into(),
                value: StatusCode::OK.as_str().into(),
            }],
            body: b"h2spec fixture".to_vec(),
            trailers: Vec::new(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    writeln!(io::stdout(), "{address}")?;

    tokio::task::LocalSet::new()
        .run_until(async move {
            loop {
                let (socket, _) = listener.accept().await?;
                tokio::task::spawn_local(async move {
                    // Conformance probes deliberately terminate connections and send invalid frames.
                    let _ = serve(FromTokio::new(socket), &mut Fixture).await;
                });
            }
        })
        .await
}
