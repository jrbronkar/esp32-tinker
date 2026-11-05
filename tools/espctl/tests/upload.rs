use ota_protocol::{UploadOperation as Op, UploadPhase, UploadReply};
use protolink::{CallContext, Handler, Next, Route, Status};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};
#[path = "support/grpc.rs"]
mod common;
use common::*;
fn image() -> Vec<u8> {
    let mut b = vec![0u8; 336];
    b[0] = 0xe9;
    b[1] = 1;
    b[12] = 9;
    b[23] = 1;
    b[28..32].copy_from_slice(&256u32.to_le_bytes());
    b[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
    b[48..50].copy_from_slice(b"v1");
    b[80..91].copy_from_slice(b"device-demo");
    b[144..144 + ota_core::MARKER.len()].copy_from_slice(ota_core::MARKER.as_bytes());
    b[303] = b[32..288].iter().fold(0xef, |a, v| a ^ v);
    let hash = ota_core::hash(&b[..304]);
    b[304..].copy_from_slice(&hash);
    b
}

#[derive(Clone)]
struct State {
    commits: Rc<Cell<usize>>,
    writes: Rc<Cell<usize>>,
    pending: Rc<Cell<bool>>,
    requests: Rc<RefCell<Vec<&'static str>>>,
    fault: &'static str,
    logs: Rc<RefCell<(ota_protocol::logs::LogBuffer, Option<std::task::Waker>)>>,
}
/// Cut the actual TCP output at a protocol milestone, before its ACK can leave.
struct CutIo {
    stream: tokio::net::TcpStream,
    state: State,
}
impl CutIo {
    fn cut(&self) -> bool {
        (self.state.fault == "disconnect-after-commit" && self.state.commits.get() != 0)
            || (self.state.fault == "disconnect-before-commit"
                && self.state.requests.borrow().last() == Some(&"finish"))
    }
}
impl tokio::io::AsyncRead for CutIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for CutIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.cut() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.cut() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
struct Memory {
    bytes: Vec<u8>,
    state: State,
}
impl ota_core::Reader for Memory {
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), ota_core::Error> {
        ota_core::SliceImage(&self.bytes).read(offset, bytes)
    }
}
impl ota_core::Target for Memory {
    fn capacity(&self) -> u32 {
        self.bytes.len() as u32
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ota_core::Error> {
        self.state.writes.set(self.state.writes.get() + 1);
        self.bytes[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn commit(&mut self) -> Result<(), ota_core::Error> {
        self.state.commits.set(self.state.commits.get() + 1);
        if self.state.fault == "commit-storage" {
            Err(ota_core::Error::Storage)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone)]
struct Env(State);
impl device_grpc::Environment for Env {
    fn status(&self) -> ota_protocol::StatusResponse {
        let mut status = ota_protocol::status("mock", "device-demo", "old", 1).unwrap();
        status.ota_available = true;
        let mut boot = ota_protocol::BootInfo::default();
        boot.set_running(ota_protocol::PartitionInfo {
            offset: 0x10000,
            ..Default::default()
        });
        status.set_boot(boot);
        status
    }
    fn read_logs(&self, cursor: &ota_protocol::ReadLogsRequest) -> ota_protocol::ReadLogsResponse {
        self.0.logs.borrow().0.read(cursor)
    }
    fn logs(
        &self,
        cursor: &ota_protocol::ReadLogsRequest,
        cx: &mut Context<'_>,
    ) -> ota_protocol::ReadLogsResponse {
        if self.0.fault == "bad-logs" {
            return Default::default();
        }
        let mut state = self.0.logs.borrow_mut();
        state.1 = Some(cx.waker().clone());
        state.0.read(cursor)
    }
    fn event(&self, event: device_grpc::UploadEvent) {
        if event == device_grpc::UploadEvent::ActivationPending {
            self.0.pending.set(true)
        }
        let mut state = self.0.logs.borrow_mut();
        state
            .0
            .push(1, ota_protocol::LogSeverity::Info, &event.to_string());
        if let Some(w) = state.1.take() {
            w.wake();
        }
    }
    fn activation_pending(&self) -> bool {
        self.0.pending.get()
    }
}
struct Faulty<'a> {
    inner: device_grpc::StreamingManagement<'a, Memory, Env>,
    state: State,
    commit_id: Option<protolink::CallId>,
}
impl Handler for Faulty<'_> {
    fn route(&self, path: &str) -> Option<Route> {
        self.inner.route(path)
    }
    fn call(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        self.inner.call(ctx, request)
    }
    fn on_message(&mut self, ctx: &mut CallContext<'_>, bytes: &[u8]) -> Result<(), Status> {
        if ctx.route.method == 1 {
            return self.inner.on_message(ctx, bytes);
        }
        let request = ota_protocol::decode_upload(bytes).unwrap();
        let name = match request.payload {
            Some(Op::Begin(_)) => "begin",
            Some(Op::Chunk(_)) => "chunk",
            Some(Op::Finish(_)) => "finish",
            Some(Op::Commit(_)) => {
                self.commit_id = Some(ctx.id);
                "commit"
            }
            Some(Op::Abort(_)) => "abort",
            _ => "other",
        };
        self.state.requests.borrow_mut().push(name);
        self.inner.on_message(ctx, bytes)
    }
    fn on_half_close(&mut self, ctx: &mut CallContext<'_>) -> Result<(), Status> {
        self.inner.on_half_close(ctx)
    }
    fn poll_response(
        &mut self,
        ctx: &mut CallContext<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<Next<Vec<u8>>> {
        let result = self.inner.poll_response(ctx, cx);
        if ctx.route.method == 1 {
            return result;
        }
        if let Poll::Ready(Next::Message(ref bytes)) = result {
            let mut response: ota_protocol::UploadResponse = ota_protocol::decode(bytes).unwrap();
            if self.commit_id == Some(ctx.id) && self.state.fault == "lost-commit" {
                return Poll::Ready(Next::Done(Err(Status::unavailable("commit ACK lost"))));
            }
            if !self.state.fault.is_empty()
                && self.state.fault != "commit-storage"
                && self.state.fault != "lost-commit"
                && self.state.fault != "bad-logs"
                && !self.state.fault.starts_with("disconnect-")
            {
                match self.state.fault {
                    "id" => response.request_id += 1,
                    "truncated" => return Poll::Ready(Next::Message(vec![0xff])),
                    "disconnect" => {
                        return Poll::Ready(Next::Done(Err(Status::unavailable("connection lost"))))
                    }
                    fault => {
                        if let Some(UploadReply::Upload(ref mut ack)) = response.payload {
                            match fault {
                                "phase" => ack.phase = UploadPhase::Verified,
                                "offset" => ack.next_offset += 1,
                                "target" => ack.target_offset = 0x9000,
                                _ => panic!(),
                            }
                        }
                    }
                }
                return Poll::Ready(Next::Message(
                    ota_protocol::encode(&response).unwrap().to_vec(),
                ));
            }
        }
        result
    }
    fn on_cancel(&mut self, ctx: &mut CallContext<'_>) {
        self.inner.on_cancel(ctx)
    }
}
async fn fixture(
    fault: &'static str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, State) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = State {
        commits: Rc::new(Cell::new(0)),
        writes: Rc::new(Cell::new(0)),
        pending: Rc::new(Cell::new(false)),
        requests: Rc::new(RefCell::new(Vec::new())),
        fault,
        logs: Rc::new(RefCell::new((ota_protocol::logs::LogBuffer::new(1), None))),
    };
    let server_state = state.clone();
    let server = tokio::task::spawn_local(async move {
        let target = RefCell::new(Memory {
            bytes: vec![0xff; 16384],
            state: server_state.clone(),
        });
        // Exactly one TCP accept, carrying both status and upload RPCs.
        let (stream, _) = listener.accept().await.unwrap();
        let mut handler = Faulty {
            inner: device_grpc::StreamingManagement::new(
                &target,
                0x310000,
                Env(server_state.clone()),
            ),
            state: server_state.clone(),
            commit_id: None,
        };
        let _ = protolink::tokio::server(CutIo {
            stream,
            state: server_state,
        })
        .with_config(device_grpc::config())
        .build()
        .serve(&mut handler)
        .await;
    });
    (address, server, state)
}
async fn client_upload(
    address: std::net::SocketAddr,
    commit: bool,
    tag: &str,
) -> std::io::Result<espctl::upload::UploadResult> {
    let path = std::env::temp_dir().join(format!("espctl-grpc-{}-{tag}.bin", std::process::id()));
    std::fs::write(&path, image()).unwrap();
    tokio::task::spawn_blocking(move || {
        let result = espctl::upload::upload(address, Duration::from_secs(2), &path, "v1", commit);
        std::fs::remove_file(path).unwrap();
        result
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn upload_verifies_and_activates_only_when_explicitly_requested() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for commit in [false, true] {
                let (address, server, state) = fixture("").await;
                let result =
                    client_upload(address, commit, if commit { "commit" } else { "verify" })
                        .await
                        .unwrap();
                assert_eq!(result.committed, commit);
                assert_eq!(result.bytes, image().len() as u32);
                assert_eq!(result.target_offset, 0x310000);
                assert_eq!(state.commits.get(), usize::from(commit));
                assert_eq!(state.pending.get(), commit);
                assert_eq!(
                    *state.requests.borrow(),
                    vec![
                        "begin",
                        "chunk",
                        "finish",
                        if commit { "commit" } else { "abort" }
                    ]
                );
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn upload_rejects_bad_acknowledgements_and_does_not_replay() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["id", "phase", "offset", "target", "truncated", "disconnect"] {
                let (address, server, state) = fixture(fault).await;
                assert!(
                    client_upload(address, true, fault).await.is_err(),
                    "{fault}"
                );
                assert_eq!(*state.requests.borrow(), vec!["begin"]);
                assert_eq!(state.commits.get(), 0);
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn lost_commit_reply_reports_uncertainty_without_replay() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["lost-commit", "commit-storage", "disconnect-after-commit"] {
                let (address, server, state) = fixture(fault).await;
                let error = client_upload(address, true, fault).await.unwrap_err();
                assert!(
                    error.to_string().contains("commit outcome uncertain"),
                    "{error}"
                );
                assert_eq!(state.commits.get(), 1);
                assert!(state.pending.get());
                assert_eq!(
                    *state.requests.borrow(),
                    vec!["begin", "chunk", "finish", "commit"]
                );
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[tokio::test]
async fn commit_before_verification_cannot_change_boot_selection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("").await;
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut client = Client::new(channel);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            assert_eq!(
                op(&tx, &mut replies, Op::Commit(Default::default()), 2)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            assert_eq!(state.commits.get(), 0);
            assert!(!state.pending.get());
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn commit_latch_prevents_any_further_upload_until_reboot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("").await;
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut client = Client::new(channel);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            op(&tx, &mut replies, begin(&image(), "v1"), 1)
                .await
                .unwrap();
            op(&tx, &mut replies, chunk(0, &image()), 2).await.unwrap();
            op(&tx, &mut replies, Op::Finish(Default::default()), 3)
                .await
                .unwrap();
            let ack = op(&tx, &mut replies, Op::Commit(Default::default()), 4)
                .await
                .unwrap();
            assert_eq!(ack.phase, UploadPhase::Committed);
            assert!(replies.message().await.unwrap().is_none());
            drop(tx);
            let (tx, mut replies) = upload(&mut client).await.unwrap();
            assert_eq!(
                op(&tx, &mut replies, begin(&image(), "v1"), 1)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            assert_eq!(state.commits.get(), 1);
            assert_eq!(state.writes.get(), 1);
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn cli_upload_logs_share_connection_and_log_errors_preserve_commit_outcome() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in ["", "bad-logs", "lost-commit"] {
                let (address, server, state) = fixture(fault).await;
                let path = std::env::temp_dir().join(format!(
                    "espctl-log-upload-{}-{fault}.bin",
                    std::process::id()
                ));
                std::fs::write(&path, image()).unwrap();
                let (result, events) = tokio::task::spawn_blocking(move || {
                    let mut events = Vec::new();
                    let result = espctl::upload::upload_with_logs(
                        address,
                        Duration::from_secs(2),
                        &path,
                        "v1",
                        true,
                        &mut |batch| events.push(batch),
                    );
                    std::fs::remove_file(path).unwrap();
                    (result, events)
                })
                .await
                .unwrap();
                if fault == "lost-commit" {
                    assert!(result
                        .unwrap_err()
                        .to_string()
                        .contains("commit outcome uncertain"));
                } else {
                    assert!(result.unwrap().committed);
                }
                if fault == "bad-logs" {
                    assert!(events.iter().any(|e| e.is_err()));
                } else {
                    assert!(events
                        .iter()
                        .filter_map(|e| e.as_ref().ok())
                        .flat_map(|b| b.entries.iter())
                        .any(|e| e.message.starts_with("gRPC chunk")));
                }
                assert_eq!(state.commits.get(), 1);
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
#[test]
fn cli_exposes_grpc_upload_logs_and_rejects_removed_legacy_option() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args(["upload", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in ["--logs", "7779"] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("--legacy"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args([
            "upload",
            "--host",
            "127.0.0.1",
            "--image",
            "missing.bin",
            "--firmware-version",
            "v1",
            "--logs",
            "--legacy",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
}
#[tokio::test]
async fn disconnect_after_verification_before_commit_never_selects_image() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, state) = fixture("disconnect-before-commit").await;
            let error = client_upload(address, true, "disconnect-before-commit")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("commit outcome uncertain"));
            assert_eq!(*state.requests.borrow(), vec!["begin", "chunk", "finish"]);
            assert_eq!(state.commits.get(), 0);
            assert!(!state.pending.get());
            server.abort();
            let _ = server.await;
        })
        .await;
}
#[tokio::test]
async fn grpc_preserves_metadata_image_digest_and_order_validation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for fault in [
                "application",
                "chip",
                "version",
                "size",
                "digest-length",
                "descriptor",
                "digest",
                "request-order",
                "early-finish",
            ] {
                let (address, server, state) = fixture("").await;
                let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = Client::new(channel);
                let (tx, mut replies) = upload(&mut client).await.unwrap();
                let bytes = image();
                let Op::Begin(mut metadata) = begin(&bytes, "v1") else {
                    panic!()
                };
                match fault {
                    "application" => metadata.application = "wrong-app".try_into().unwrap(),
                    "chip" => metadata.chip = "esp32".try_into().unwrap(),
                    "version" => metadata.version.clear(),
                    "size" => metadata.image_size = ota_core::SLOT_SIZE + 1,
                    "digest-length" => metadata.sha256.clear(),
                    "digest" => metadata.sha256[0] ^= 1,
                    _ => {}
                }
                let result = op(&tx, &mut replies, Op::Begin(metadata), 1).await;
                if ["application", "chip", "version", "size", "digest-length"].contains(&fault) {
                    assert_eq!(
                        result.unwrap_err().code(),
                        tonic::Code::FailedPrecondition,
                        "{fault}"
                    );
                    assert_eq!(state.writes.get(), 0);
                } else {
                    result.unwrap();
                    let result = match fault {
                        "descriptor" => {
                            let mut bad = bytes.clone();
                            bad[144] ^= 1;
                            op(&tx, &mut replies, chunk(0, &bad), 2).await
                        }
                        "request-order" => op(&tx, &mut replies, chunk(0, &bytes), 3).await,
                        "early-finish" => {
                            op(&tx, &mut replies, Op::Finish(Default::default()), 2).await
                        }
                        "digest" => {
                            op(&tx, &mut replies, chunk(0, &bytes), 2).await.unwrap();
                            op(&tx, &mut replies, Op::Finish(Default::default()), 3).await
                        }
                        _ => unreachable!(),
                    };
                    assert!(result.is_err(), "{fault}");
                    assert_eq!(state.writes.get(), usize::from(fault == "digest"));
                }
                assert_eq!(state.commits.get(), 0);
                assert!(!state.pending.get());
                server.abort();
                let _ = server.await;
            }
        })
        .await;
}
