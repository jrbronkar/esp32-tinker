mod common;
use common::*;
use ota_protocol::{RequestPayload as Op, ResponsePayload};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: concurrent BOARD_IP IMAGE.bin FIRMWARE_VERSION (verify only)".into());
    }
    let image = std::fs::read(&args[2])?;
    ota_core::validate_image(
        &mut ota_core::SliceImage(&image),
        image.len() as u32,
        &args[3],
    )
    .map_err(|e| format!("invalid image: {e:?}"))?;
    tokio::time::timeout(Duration::from_secs(170), async {
        let channel=tonic::transport::Endpoint::from_shared(format!("http://{}:7780",args[1]))?.initial_stream_window_size(Some(1024)).connect().await?;
        let mut client=Client::new(channel);
        let stalled = std::env::var_os("GRPC_STALL_LOGS").is_some();
        client.ready().await?;
        let reply=client.unary(tonic::Request::new(ota_protocol::encode(&ota_protocol::status_request(1)).map_err(|e|format!("encode: {e:?}"))?.to_vec()),device_grpc::STATUS_PATH.parse()?,RawCodec).await?;
        let status=response(&reply.into_inner());
        let Some(ResponsePayload::Status(status))=status.payload else {return Err("missing status".into())};
        if status.device_id!="esp32s3-a4:cb:8f:d8:13:c8" {return Err(format!("unexpected board: {}",status.device_id).into())}
        println!("Verified identity {} running {}; ONE Tonic channel / HTTP2 connection",status.device_id,status.firmware_version);
        let mut logs=logs(&mut client,Default::default()).await?;
        logs.message().await?.ok_or("missing initial log snapshot")?;
        let (tx,mut replies)=upload(&mut client).await?;
        let mut id=(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u32) & 0x7fff_ffff;
        let ack = op(&tx, &mut replies, begin(&image, &args[3]), id).await?;
        let running = status.boot().and_then(|boot| boot.running()).ok_or("missing boot inventory")?.offset;
        let expected_target = if running == 0x10000 { 0x310000 } else if running == 0x310000 { 0x10000 } else { return Err("unexpected running slot".into()) };
        if ack.target_offset != expected_target { return Err("wrong inactive target offset".into()); }
        id+=1;
        let started=Instant::now(); let mut count=0; let mut max_wait=Duration::ZERO;
        for (i,bytes) in image.chunks(4096).enumerate() {
            let step=Instant::now();
            let ack=op(&tx,&mut replies,chunk(i*4096,bytes),id).await?;
            if ack.next_offset as usize != i*4096+bytes.len() {return Err("wrong offset".into())}
            let expected=format!("gRPC chunk id={} next={}",id,ack.next_offset); id+=1;
            if !stalled { loop {
                let bytes=logs.message().await?.ok_or("logs ended during upload")?;
                let Some(ResponsePayload::Logs(batch))=response(&bytes).payload else {return Err("wrong log response".into())};
                if batch.entries.iter().any(|e|e.message.as_str()==expected) { break; }
            }
            }
            max_wait=max_wait.max(step.elapsed()); count+=1;
            if count%16==0 {
                if stalled { println!("{} bytes written while log consumption is paused",ack.next_offset); }
                else { println!("{} bytes written, {} live chunk logs received before next chunk",ack.next_offset,count); }
            }
        }
        let ack=op(&tx,&mut replies,Op::Finish(Default::default()),id).await?;id+=1;
        if ack.phase!=ota_protocol::UploadPhase::Verified {return Err("image not verified".into())}
        op(&tx,&mut replies,Op::Abort(Default::default()),id).await?;
        if replies.message().await?.is_some() {return Err("unexpected trailing message".into())}
        if stalled {
            loop {
                let bytes=logs.message().await?.ok_or("log stream ended")?;
                let Some(ResponsePayload::Logs(batch))=response(&bytes).payload else {return Err("wrong log response".into())};
                if batch.dropped {break;}
            }
            println!("PASS: log receive window stalled at 1024 bytes; upload still verified; overwritten history reported on resumption");
        }
        println!("PASS: {} bytes verified from flash; {} chunks; {} matching live logs DURING upload; elapsed {:?}; max chunk step {:?}; no activation",image.len(),count,if stalled {0} else {count},started.elapsed(),max_wait);
        Ok::<(),Box<dyn std::error::Error>>(())
    }).await??;
    Ok(())
}
