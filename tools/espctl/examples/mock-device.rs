//! Host-only stand-in for protocol/client iteration; does not emulate radio or flash.
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

fn main() -> std::io::Result<()> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7779".into());
    let listener = TcpListener::bind(&address)?;
    println!(
        "MOCK device listening on {} (not real hardware)",
        listener.local_addr()?
    );
    let started = Instant::now();
    for socket in listener.incoming() {
        let mut socket = socket?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut prefix = [0; 4];
            socket.read_exact(&mut prefix)?;
            let len = ota_protocol::frame_length(prefix).map_err(|e| format!("{e:?}"))?;
            let mut body = [0; ota_protocol::MAX_FRAME];
            socket.read_exact(&mut body[..len])?;
            let request = ota_protocol::decode(&body[..len]).map_err(|e| format!("{e:?}"))?;
            let status = ota_protocol::status(
                "mock-esp32s3",
                "device-demo",
                "mock-v1",
                started.elapsed().as_millis() as u64,
            )
            .unwrap();
            let body = ota_protocol::encode(&ota_protocol::respond(request, status))
                .map_err(|e| format!("{e:?}"))?;
            socket.write_all(&(body.len() as u32).to_be_bytes())?;
            socket.write_all(&body)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("mock session: {error}");
        }
    }
    Ok(())
}
