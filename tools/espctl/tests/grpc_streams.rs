#[path = "support/grpc.rs"]
mod common;
use common::*;
use embedded_io_adapters::tokio_1::FromTokio;
use ota_protocol::{ReadLogsRequest, UploadOperation as Op};
use std::{cell::RefCell, rc::Rc, task::Context, time::Duration};

struct Memory(Vec<u8>);
impl ota_core::Reader for Memory {
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), ota_core::Error> {
        ota_core::SliceImage(&self.0).read(offset, bytes)
    }
}
impl ota_core::Target for Memory {
    fn capacity(&self) -> u32 {
        self.0.len() as u32
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ota_core::Error> {
        self.0[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn commit(&mut self) -> Result<(), ota_core::Error> {
        panic!("verify-only transport must never commit")
    }
}
struct Events {
    logs: device_grpc::logs::LogStore<2>,
    watchdog: Rc<device_grpc::watchdog::UploadWatchdog>,
    timer: protolink::tokio::TokioTimer,
}
#[derive(Clone)]
struct Env(Rc<RefCell<Events>>);
impl device_grpc::Environment for Env {
    fn status(&self) -> ota_protocol::StatusResponse {
        ota_protocol::status("mock", "device-demo", "v1", 0).unwrap()
    }
    fn logs(
        &self,
        cursor: &ReadLogsRequest,
        cx: &mut Context<'_>,
    ) -> ota_protocol::ReadLogsResponse {
        self.0.borrow().logs.read_and_subscribe(cursor, cx)
    }
    fn event(&self, event: device_grpc::UploadEvent) {
        {
            let env = self.0.borrow();
            match event {
                device_grpc::UploadEvent::Receiving
                | device_grpc::UploadEvent::ChunkWritten { .. }
                | device_grpc::UploadEvent::Verified => {
                    env.watchdog.activity(protolink::Clock::now(&env.timer))
                }
                device_grpc::UploadEvent::Released
                | device_grpc::UploadEvent::ActivationPending => env.watchdog.clear(),
                device_grpc::UploadEvent::Committed => {}
            }
        }
        self.log(&event.to_string());
    }
}
impl Env {
    fn log(&self, message: &str) {
        self.0
            .borrow()
            .logs
            .push(0, ota_protocol::LogSeverity::Info, message);
    }
}

fn image() -> Vec<u8> {
    let size = 128 * 1024;
    let end = 32 + size;
    let footer = (end + 16) & !15;
    let mut bytes = vec![0u8; footer + 32];
    bytes[0] = 0xe9;
    bytes[1] = 1;
    bytes[12] = 9;
    bytes[23] = 1;
    bytes[28..32].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
    bytes[48..50].copy_from_slice(b"v1");
    bytes[80..91].copy_from_slice(b"device-demo");
    bytes[144..144 + ota_core::MARKER.len()].copy_from_slice(ota_core::MARKER.as_bytes());
    bytes[footer - 1] = bytes[32..end].iter().fold(0xef, |a, b| a ^ b);
    let digest = ota_core::hash(&bytes[..footer]);
    bytes[footer..].copy_from_slice(&digest);
    bytes
}
async fn fixture() -> (
    Client,
    tokio::task::JoinHandle<()>,
    Rc<RefCell<Memory>>,
    Env,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let memory = Rc::new(RefCell::new(Memory(vec![0xff; 256 * 1024])));
    let env = Env(Rc::new(RefCell::new(Events {
        logs: device_grpc::logs::LogStore::new(1),
        watchdog: Rc::new(device_grpc::watchdog::UploadWatchdog::new()),
        timer: protolink::tokio::TokioTimer::new(),
    })));
    let target = memory.clone();
    let events = env.clone();
    let server = tokio::task::spawn_local(async move {
        // Exactly one accept: every RPC in the test must share this TCP connection.
        let (stream, _) = listener.accept().await.unwrap();
        serve(stream, &target, events).await;
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .initial_stream_window_size(Some(1024))
        .connect()
        .await
        .unwrap();
    (Client::new(channel), server, memory, env)
}
async fn serve(stream: tokio::net::TcpStream, target: &RefCell<Memory>, env: Env) {
    let watchdog = env.0.borrow().watchdog.clone();
    let timer = env.0.borrow().timer;
    let handler = management_grpc::ManagementServer::new(device_grpc::EmbeddedManagement::new(
        target, 0x10000, env,
    ));
    let connection = embedded_grpc::serve_streaming(FromTokio::new(stream), &handler);
    embassy_futures::select::select(connection, watchdog.expired(&timer)).await;
}

#[tokio::test]
async fn same_connection_upload_and_live_logs_verify_image() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                let (mut client, server, memory, _) = fixture().await;
                let mut logs = logs(&mut client, ReadLogsRequest::default()).await.unwrap();
                logs.message().await.unwrap().unwrap(); // initial cursor snapshot
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                let image = image();
                op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                let mut id = 2;
                let mut during_upload = 0;
                for (i, bytes) in image.chunks(4096).enumerate() {
                    let ack = op(&tx, &mut replies, chunk(i * 4096, bytes), id)
                        .await
                        .unwrap();
                    id += 1;
                    assert_eq!(ack.next_offset as usize, (i * 4096) + bytes.len());
                    // Wait for real chunk events BEFORE the next chunk/Finish. A sequential
                    // upload-then-log server cannot pass, even if it eventually returns logs.
                    loop {
                        let batch = log_response(&logs.message().await.unwrap().unwrap());
                        if batch.entries.iter().any(|e| {
                            e.message.as_str()
                                == format!("gRPC chunk id={} next={}", id - 1, ack.next_offset)
                        }) {
                            during_upload += 1;
                            break;
                        }
                    }
                }
                let ack = op(&tx, &mut replies, Op::Finish(Default::default()), id)
                    .await
                    .unwrap();
                id += 1;
                assert_eq!(ack.phase, ota_protocol::UploadPhase::Verified);
                assert_eq!(during_upload, image.chunks(4096).count());
                op(&tx, &mut replies, Op::Abort(Default::default()), id)
                    .await
                    .unwrap();
                assert!(replies.message().await.unwrap().is_none());
                assert_eq!(&memory.borrow().0[..image.len()], &image);
                drop(logs);
                drop(tx);
                server.abort();
                let _ = server.await;
            })
            .await
            .unwrap();
        })
        .await;
}
#[tokio::test]
async fn cancellation_errors_and_disconnect_release_flash() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let (mut client, server, memory, env) = fixture().await;
                let image = image();
                for _ in 0..4 {
                    let (tx, mut replies) = upload(&mut client).await.unwrap();
                    op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                    assert!(memory.try_borrow_mut().is_err());
                    drop(replies);
                    drop(tx);
                    // A successful new Begin below proves cleanup, not just client drop.
                    for _ in 0..100 {
                        if memory.try_borrow_mut().is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    assert!(memory.try_borrow_mut().is_ok());
                }
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                assert_eq!(
                    op(&tx, &mut replies, chunk(4096, &image[..4096]), 2)
                        .await
                        .unwrap_err()
                        .code(),
                    tonic::Code::FailedPrecondition
                );
                assert!(memory.try_borrow_mut().is_ok());
                drop(tx);
                drop(replies);
                // A decode error happens before the generated service method runs.
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                tx.send(vec![0xff]).await.unwrap();
                assert_eq!(
                    replies.message().await.unwrap_err().code(),
                    tonic::Code::InvalidArgument
                );
                assert!(memory.try_borrow_mut().is_ok());
                drop(tx);
                drop(replies);
                // New subscriptions wake after an external log producer, and cancel cleanly.
                for _ in 0..4 {
                    let mut cursor = ReadLogsRequest::default();
                    loop {
                        let batch = env.0.borrow().logs.read(&cursor);
                        cursor.boot_id = batch.boot_id;
                        cursor.after_sequence = batch.next_sequence;
                        if batch.entries.is_empty() {
                            break;
                        }
                    }
                    let mut stream = logs(&mut client, cursor).await.unwrap();
                    stream.message().await.unwrap().unwrap();
                    env.log("external wake");
                    let batch = log_response(&stream.message().await.unwrap().unwrap());
                    assert!(batch.entries.iter().any(|e| e.message == "external wake"));
                    drop(stream);
                }
                let (empty_tx, mut empty) = upload(&mut client).await.unwrap();
                drop(empty_tx);
                assert_eq!(
                    empty.message().await.unwrap_err().code(),
                    tonic::Code::FailedPrecondition
                );

                let (active_tx, mut active) = upload(&mut client).await.unwrap();
                op(&active_tx, &mut active, begin(&image, "v1"), 1)
                    .await
                    .unwrap();
                assert!(memory.try_borrow_mut().is_err());
                server.abort();
                let _ = server.await;
                assert!(memory.try_borrow_mut().is_ok());
            })
            .await
            .unwrap();
        })
        .await;
}
#[tokio::test]
async fn stalled_log_stream_does_not_stall_upload_and_reports_overwrite() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                let (mut client, server, memory, _) = fixture().await;
                let mut logs = logs(&mut client, Default::default()).await.unwrap();
                logs.message().await.unwrap().unwrap();
                // Peer grants just 1024 bytes per stream. Do NOT consume logs while
                // producing substantially more than that; uploads must still finish.
                let image = image();
                for _ in 0..3 {
                    let (tx, mut replies) = upload(&mut client).await.unwrap();
                    op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                    let mut id = 2;
                    for (i, bytes) in image.chunks(4096).enumerate() {
                        op(&tx, &mut replies, chunk(i * 4096, bytes), id)
                            .await
                            .unwrap();
                        id += 1;
                    }
                    let ack = op(&tx, &mut replies, Op::Finish(Default::default()), id)
                        .await
                        .unwrap();
                    id += 1;
                    assert_eq!(ack.phase, ota_protocol::UploadPhase::Verified);
                    op(&tx, &mut replies, Op::Abort(Default::default()), id)
                        .await
                        .unwrap();
                    assert!(replies.message().await.unwrap().is_none());
                }
                assert_eq!(&memory.borrow().0[..image.len()], &image);
                loop {
                    let batch = log_response(&logs.message().await.unwrap().unwrap());
                    if batch.dropped {
                        break;
                    }
                }
                drop(logs);
                server.abort();
                let _ = server.await;
            })
            .await
            .unwrap();
        })
        .await;
}
#[tokio::test]
async fn competing_stream_cannot_release_active_upload() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let (mut client, server, memory, _) = fixture().await;
                let image = image();
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                op(&tx, &mut replies, begin(&image, "v1"), 1).await.unwrap();
                let (other_tx, mut other) = upload(&mut client).await.unwrap();
                assert_eq!(
                    op(&other_tx, &mut other, begin(&image, "v1"), 1)
                        .await
                        .unwrap_err()
                        .code(),
                    tonic::Code::ResourceExhausted
                );
                assert!(memory.try_borrow_mut().is_err());
                op(&tx, &mut replies, chunk(0, &image[..4096]), 2)
                    .await
                    .unwrap();
                op(&tx, &mut replies, Op::Abort(Default::default()), 3)
                    .await
                    .unwrap();
                assert!(replies.message().await.unwrap().is_none());
                drop(other_tx);
                drop(other);
                drop(tx);
                server.abort();
                let _ = server.await;
                assert!(memory.try_borrow_mut().is_ok());
            })
            .await
            .unwrap();
        })
        .await;
}
// Keep testing the deadline behavior of the legacy transport while it remains
// in the workspace. The embedded transport does not yet interpret grpc-timeout.
#[tokio::test]
async fn protolink_deadline_cancels_logs() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::task::spawn_local(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let target = RefCell::new(Memory(vec![0xff; 256 * 1024]));
                    let env = Env(Rc::new(RefCell::new(Events {
                        logs: device_grpc::logs::LogStore::new(1),
                        watchdog: Rc::new(device_grpc::watchdog::UploadWatchdog::new()),
                        timer: protolink::tokio::TokioTimer::new(),
                    })));
                    let mut handler = device_grpc::ManagementServer::new(
                        device_grpc::StreamingManagement::new(&target, 0x10000, env),
                    );
                    let _ = protolink::tokio::server(stream)
                        .with_config(device_grpc::config())
                        .build()
                        .serve(&mut handler)
                        .await;
                });
                let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = Client::new(channel);
                client.ready().await.unwrap();
                let mut request =
                    tonic::Request::new(proto(&ota_protocol::ReadLogsRequest::default()));
                request.set_timeout(Duration::from_millis(100));
                let mut stream = client
                    .server_streaming(request, device_grpc::LOGS_PATH.parse().unwrap(), RawCodec)
                    .await
                    .unwrap()
                    .into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(_)) => {}
                        Err(status) => {
                            assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
                            break;
                        }
                        Ok(None) => panic!("deadline must have error trailers"),
                    }
                }
                server.abort();
                let _ = server.await;
            })
            .await
            .unwrap();
        })
        .await;
}

#[tokio::test]
async fn live_log_traffic_cannot_keep_an_abandoned_upload_holding_flash() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut client, server, memory, env) = fixture().await;
            let mut logs = logs(&mut client, Default::default()).await.unwrap();
            logs.message().await.unwrap().unwrap();
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            assert!(memory.try_borrow_mut().is_err());
            let started = std::time::Instant::now();
            let mut count = 0;
            tokio::time::timeout(Duration::from_secs(7), async {
                loop {
                    env.log("external live log");
                    match logs.message().await {
                        Ok(Some(_)) => count += 1,
                        _ => break,
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
            assert!(count > 10);
            assert!(started.elapsed() >= Duration::from_secs(4));
            server.await.unwrap();
            assert!(memory.try_borrow_mut().is_ok());
            drop(tx);
            drop(replies);
        })
        .await;
}
#[tokio::test]
async fn cancelled_snapshot_does_not_interrupt_another_log_stream() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut client, server, _, env) = fixture().await;
            let mut stream = logs(&mut client, Default::default()).await.unwrap();
            stream.message().await.unwrap().unwrap();
            client.ready().await.unwrap();
            let mut snapshot = logs(&mut client, Default::default()).await.unwrap();
            assert_ne!(
                log_response(&snapshot.message().await.unwrap().unwrap()).boot_id,
                0
            );
            drop(snapshot);
            env.log("after snapshot");
            let bytes = tokio::time::timeout(Duration::from_secs(1), stream.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let batch = log_response(&bytes);
            assert!(batch.entries.iter().any(|e| e.message == "after snapshot"));
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn two_connections_share_exclusive_flash_without_sharing_idle_timeouts() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let target = Rc::new(RefCell::new(Memory(vec![0xff; 256 * 1024])));
            let flash = target.clone();
            let server = tokio::task::spawn_local(async move {
                let mut sessions = tokio::task::JoinSet::new();
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let flash = flash.clone();
                    sessions.spawn_local(async move {
                        let env = Env(Rc::new(RefCell::new(Events {
                            logs: device_grpc::logs::LogStore::new(1),
                            watchdog: Rc::new(device_grpc::watchdog::UploadWatchdog::new()),
                            timer: protolink::tokio::TokioTimer::new(),
                        })));
                        serve(stream, &flash, env).await;
                    });
                }
                while sessions.join_next().await.is_some() {}
            });
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut first = Client::new(channel);
            let (tx, mut pending) = upload(&mut first).await.unwrap();
            op(&tx, &mut pending, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut second = Client::new(channel);
            second.ready().await.unwrap();
            second
                .unary(
                    tonic::Request::new(proto(&ota_protocol::StatusRequest::default())),
                    device_grpc::STATUS_PATH.parse().unwrap(),
                    RawCodec,
                )
                .await
                .unwrap();
            let (other_tx, mut other) = upload(&mut second).await.unwrap();
            assert_eq!(
                op(&other_tx, &mut other, begin(&image(), "v1"), 1)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::ResourceExhausted
            );
            drop(other_tx);
            drop(other);
            assert!(
                tokio::time::timeout(Duration::from_secs(7), pending.message())
                    .await
                    .unwrap()
                    .is_err()
            );
            drop(tx);
            drop(first);
            // The abandoned first upload must not close the second connection.
            second.ready().await.unwrap();
            second
                .unary(
                    tonic::Request::new(proto(&ota_protocol::StatusRequest::default())),
                    device_grpc::STATUS_PATH.parse().unwrap(),
                    RawCodec,
                )
                .await
                .unwrap();
            let (tx, mut replies) = upload(&mut second).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            op(&tx, &mut replies, Op::Abort(Default::default()), 2)
                .await
                .unwrap();
            assert!(replies.message().await.unwrap().is_none());
            assert!(target.try_borrow_mut().is_ok());
            server.abort();
            let _ = server.await;
        })
        .await;
}
