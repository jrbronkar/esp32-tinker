#[path = "../tests/support/grpc.rs"]
mod common;
use common::*;
use ota_protocol::{RequestPayload as Op, ResponsePayload};
use std::time::Duration;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: grpc-cleanup BOARD_IP IMAGE.bin VERSION".into());
    }
    let image = std::fs::read(&args[2])?;
    tokio::time::timeout(Duration::from_secs(15),async {
        let channel=tonic::transport::Endpoint::from_shared(format!("http://{}:7780",args[1]))?.connect().await?;
        let mut client=Client::new(channel);
        client.ready().await?;
        let reply=client.unary(tonic::Request::new(body(Op::Status(Default::default()),1)),device_grpc::STATUS_PATH.parse()?,RawCodec).await?;
        let Some(ResponsePayload::Status(status))=response(&reply.into_inner()).payload else {return Err("missing identity".into())};
        if status.device_id!="esp32s3-a4:cb:8f:d8:13:c8" {return Err("unexpected board identity".into())}
        for _ in 0..4 {
            let mut logs=logs(&mut client,Default::default()).await?;
            logs.message().await?.ok_or("missing initial logs")?;
            let (tx,mut replies)=upload(&mut client).await?;
            op(&tx,&mut replies,begin(&image,&args[3]),1).await?;
            drop(replies);drop(tx);drop(logs);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // A subsequent successful Begin demonstrates release of the previous lease.
        let (tx,mut replies)=upload(&mut client).await?;
        op(&tx,&mut replies,begin(&image,&args[3]),1).await?;
        op(&tx,&mut replies,Op::Abort(Default::default()),2).await?;
        if replies.message().await?.is_some() {return Err("extra ack".into())}
        drop(tx);
        let (tx,mut empty)=upload(&mut client).await?; drop(tx);
        match empty.message().await {
            Err(e) if e.code()==tonic::Code::FailedPrecondition=>{},
            other=>return Err(format!("unexpected empty upload result {other:?}").into()),
        }
        println!("PASS: four upload cancellations and log resubscriptions; subsequent Begin/Abort succeeds; empty upload rejected; one connection");
        Ok::<(),Box<dyn std::error::Error>>(())
    }).await??;
    Ok(())
}
