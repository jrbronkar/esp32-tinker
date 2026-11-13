use std::{
    io::{self, Write},
    net::Ipv4Addr,
};

use embedded_http2::{
    HeaderField, Server,
    service::{Request, Response, Service},
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

    loop {
        let (socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Ok(mut server) = Server::handshake(FromTokio::new(socket)).await {
                // Conformance probes deliberately terminate connections and send invalid frames.
                let _ = server.serve(&mut Fixture).await;
            }
        });
    }
}
