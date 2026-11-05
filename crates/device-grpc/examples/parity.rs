//! Hardware regression probe for upload inactivity and removal of legacy writes.
mod common;
use common::*;
use ota_protocol::{RequestPayload as Op, ResponsePayload};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn connect(host: &str) -> Result<Client, Box<dyn std::error::Error>> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{host}:7780"))?
        .connect_timeout(Duration::from_secs(2))
        .http2_keep_alive_interval(Duration::from_millis(100))
        .keep_alive_while_idle(true)
        .connect()
        .await?;
    Ok(Client::new(channel))
}
async fn identity(client: &mut Client) -> Result<(), Box<dyn std::error::Error>> {
    client.ready().await?;
    let reply = client
        .unary(
            tonic::Request::new(body(Op::Status(Default::default()), 1)),
            device_grpc::STATUS_PATH.parse()?,
            RawCodec,
        )
        .await?;
    let Some(ResponsePayload::Status(status)) = response(&reply.into_inner()).payload else {
        return Err("missing status".into());
    };
    if status.device_id != "esp32s3-a4:cb:8f:d8:13:c8" {
        return Err("unexpected board identity".into());
    }
    println!(
        "Verified {} running {}",
        status.device_id, status.firmware_version
    );
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: parity BOARD_IP IMAGE.bin VERSION".into());
    }
    let image = std::fs::read(&args[2])?;
    ota_core::validate_image(
        &mut ota_core::SliceImage(&image),
        image.len() as u32,
        &args[3],
    )
    .map_err(|e| format!("invalid image: {e:?}"))?;
    tokio::time::timeout(Duration::from_secs(30),async {
        let mut client=connect(&args[1]).await?;identity(&mut client).await?;
        let mut logs=logs(&mut client,Default::default()).await?;logs.message().await?.ok_or("missing initial log batch")?;
        let (tx,mut replies)=upload(&mut client).await?;op(&tx,&mut replies,begin(&image,&args[3]),1).await?;
        let started=Instant::now();
        // HTTP/2 keepalive PINGs keep TCP active, while the upload sends no ops.
        match tokio::time::timeout(Duration::from_secs(8),replies.message()).await? {
            Err(_)|Ok(None)=>{},Ok(Some(_))=>return Err("unexpected idle upload response".into()),
        }
        if started.elapsed()<Duration::from_secs(4){return Err("connection ended before upload idle deadline".into())}
        println!("PASS: idle upload closed after {:?} despite HTTP/2 keepalives and open Logs stream",started.elapsed());
        drop(logs);drop(replies);drop(tx);drop(client);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut client=connect(&args[1]).await?;
        let (tx,mut replies)=upload(&mut client).await?;op(&tx,&mut replies,begin(&image,&args[3]),1).await?;
        op(&tx,&mut replies,Op::Abort(Default::default()),2).await?;
        if replies.message().await?.is_some(){return Err("unexpected abort response".into())}
        drop(replies);drop(tx);drop(client);
        println!("PASS: reconnect and Begin/Abort prove exclusive flash lease released");
        let operations=[begin(&image,&args[3]),chunk(0,&[1]),Op::Finish(Default::default()),Op::Commit(Default::default()),Op::Abort(Default::default())];
        for (index,payload) in operations.into_iter().enumerate() {
            let connect_deadline=Instant::now()+Duration::from_secs(2);
            let mut socket=loop {
                match tokio::net::TcpStream::connect(format!("{}:7779",args[1])).await {
                    Ok(socket)=>break socket,
                    Err(error) if error.kind()==std::io::ErrorKind::ConnectionRefused && Instant::now()<connect_deadline=>tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error)=>return Err(error.into()),
                }
            };
            let id=(index+1) as u32;let bytes=body(payload,id);
            socket.write_all(&(bytes.len() as u32).to_be_bytes()).await?;socket.write_all(&bytes).await?;
            let mut prefix=[0;4];socket.read_exact(&mut prefix).await?;
            let size=ota_protocol::frame_length(prefix).map_err(|_|"invalid read-only response length")?;
            let mut bytes=vec![0;size];socket.read_exact(&mut bytes).await?;
            let response=response(&bytes);
            if response.protocol_version!=1||response.request_id!=id||!matches!(response.payload,Some(ResponsePayload::Error(ref e)) if e.code==ota_protocol::ErrorCode::InvalidRequest){return Err("legacy upload operation was not rejected".into())}
        }
        println!("PASS: TCP 7779 rejects Begin, Chunk, Finish, Commit and Abort; remains read-only");
        Ok::<(),Box<dyn std::error::Error>>(())
    }).await??;
    Ok(())
}
