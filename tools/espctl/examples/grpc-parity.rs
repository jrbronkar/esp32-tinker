//! Hardware regression probe for upload inactivity and removal of legacy writes.
#[path = "../tests/support/grpc.rs"]
mod common;
use common::*;
use ota_protocol::UploadOperation as Op;
use std::time::{Duration, Instant};

async fn connect(host: &str) -> Result<Client, Box<dyn std::error::Error>> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{host}:7779"))?
        .connect_timeout(Duration::from_secs(2))
        .http2_keep_alive_interval(Duration::from_millis(100))
        .keep_alive_while_idle(true)
        .connect()
        .await?;
    Ok(Client::new(channel))
}
async fn identity(
    client: &mut Client,
) -> Result<ota_protocol::StatusResponse, Box<dyn std::error::Error>> {
    client.ready().await?;
    let reply = client
        .unary(
            tonic::Request::new(proto(&ota_protocol::StatusRequest::default())),
            device_grpc::STATUS_PATH.parse()?,
            RawCodec,
        )
        .await?;
    let status = status_response(&reply.into_inner());
    if status.device_id != "esp32s3-a4:cb:8f:d8:13:c8" {
        return Err("unexpected board identity".into());
    }
    println!(
        "Verified {} running {}",
        status.device_id, status.firmware_version
    );
    Ok(status)
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: grpc-parity BOARD_IP IMAGE.bin VERSION".into());
    }
    let image = std::fs::read(&args[2])?;
    ota_core::validate_image(
        &mut ota_core::SliceImage(&image),
        image.len() as u32,
        &args[3],
    )
    .map_err(|e| format!("invalid image: {e:?}"))?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut client = connect(&args[1]).await?;
        let application = identity(&mut client).await?.application;
        let mut logs = logs(&mut client, Default::default()).await?;
        logs.message().await?.ok_or("missing initial log batch")?;
        let (tx, mut replies) = upload(&mut client).await?;
        op(
            &tx,
            &mut replies,
            begin_for_application(&image, &args[3], application.as_str()),
            1,
        )
        .await?;
        let started = Instant::now();
        let mut second = connect(&args[1]).await?;
        tokio::time::timeout(Duration::from_secs(2), identity(&mut second)).await??;
        let (other_tx, mut other) = upload(&mut second).await?;
        match op(
            &other_tx,
            &mut other,
            begin_for_application(&image, &args[3], application.as_str()),
            1,
        )
        .await
        {
            Err(error) if error.code() == tonic::Code::ResourceExhausted => {}
            _ => return Err("concurrent upload was not rejected".into()),
        }
        drop(other_tx);
        drop(other);
        drop(second);
        println!(
            "PASS: another client can query status while upload owns flash; second upload rejected"
        );
        // HTTP/2 keepalive PINGs keep TCP active, while the upload sends no ops.
        match tokio::time::timeout(Duration::from_secs(8), replies.message()).await? {
            Err(_) | Ok(None) => {}
            Ok(Some(_)) => return Err("unexpected idle upload response".into()),
        }
        if started.elapsed() < Duration::from_secs(4) {
            return Err("connection ended before upload idle deadline".into());
        }
        println!(
            "PASS: idle upload closed after {:?} despite HTTP/2 keepalives and open Logs stream",
            started.elapsed()
        );
        drop(logs);
        drop(replies);
        drop(tx);
        drop(client);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut client = connect(&args[1]).await?;
        let (tx, mut replies) = upload(&mut client).await?;
        op(
            &tx,
            &mut replies,
            begin_for_application(&image, &args[3], application.as_str()),
            1,
        )
        .await?;
        op(&tx, &mut replies, Op::Abort(Default::default()), 2).await?;
        if replies.message().await?.is_some() {
            return Err("unexpected abort response".into());
        }
        drop(replies);
        drop(tx);
        drop(client);
        println!("PASS: reconnect and Begin/Abort prove exclusive flash lease released");
        match tokio::net::TcpStream::connect(format!("{}:7780", args[1])).await {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            _ => return Err("retired TCP port 7780 is still reachable".into()),
        }
        println!("PASS: only TCP 7779 exposes Management; retired port 7780 is closed");
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}
