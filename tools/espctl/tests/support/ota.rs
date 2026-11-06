//! Loopback gRPC board with the firmware management handler and RAM-backed flash.
use ota_protocol::{MessageDecode, UploadOperation as Op, UploadPhase, UploadRequest};
use protolink::{CallContext, Handler, Next, Route, Status};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    task::{Context, Poll},
};
pub fn image() -> Vec<u8> {
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
pub struct State {
    pub commits: Rc<Cell<usize>>,
    pub writes: Rc<Cell<usize>>,
    pub pending: Rc<Cell<bool>>,
    pub requests: Rc<RefCell<Vec<&'static str>>>,
    fault: &'static str,
    reboot: bool,
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
        let activated = self.0.reboot && self.0.pending.get();
        let version = if activated { "v1" } else { "old" };
        let mut status = ota_protocol::status("mock", "device-demo", version, 1).unwrap();
        status.ota_available = true;
        let mut boot = ota_protocol::BootInfo::default();
        boot.set_running(ota_protocol::PartitionInfo {
            offset: if activated { 0x310000 } else { 0x10000 },
            ..Default::default()
        });
        status.set_boot(boot);
        status
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
    inner: device_grpc::ManagementServer<device_grpc::StreamingManagement<'a, Memory, Env>>,
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
        if ctx.path == device_grpc::LOGS_PATH {
            return self.inner.on_message(ctx, bytes);
        }
        let mut request = UploadRequest::default();
        request.decode_from_bytes(bytes).unwrap();
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
        if ctx.path == device_grpc::LOGS_PATH {
            return result;
        }
        if let Poll::Ready(Next::Message(ref bytes)) = result {
            let mut response: ota_protocol::UploadAck = ota_protocol::decode(bytes).unwrap();
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
                        let ack = &mut response;
                        match fault {
                            "phase" => ack.phase = UploadPhase::Verified,
                            "offset" => ack.next_offset += 1,
                            "target" => ack.target_offset = 0x9000,
                            _ => panic!(),
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
pub async fn start(
    fault: &'static str,
    reboot: bool,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, State) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = State {
        commits: Rc::new(Cell::new(0)),
        writes: Rc::new(Cell::new(0)),
        pending: Rc::new(Cell::new(false)),
        requests: Rc::new(RefCell::new(Vec::new())),
        fault,
        reboot,
        logs: Rc::new(RefCell::new((ota_protocol::logs::LogBuffer::new(1), None))),
    };
    let server_state = state.clone();
    let server = tokio::task::spawn_local(async move {
        let target = Rc::new(RefCell::new(Memory {
            bytes: vec![0xff; 16384],
            state: server_state.clone(),
        }));
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            if !reboot {
                // The CLI tests expect all RPCs to use a single connection.
                serve(stream, server_state, target).await;
                break;
            }
            tokio::task::spawn_local(serve(stream, server_state.clone(), target.clone()));
        }
    });
    (address, server, state)
}

async fn serve(stream: tokio::net::TcpStream, state: State, target: Rc<RefCell<Memory>>) {
    let mut handler = Faulty {
        inner: device_grpc::ManagementServer::new(device_grpc::StreamingManagement::new(
            target.as_ref(),
            0x310000,
            Env(state.clone()),
        )),
        state: state.clone(),
        commit_id: None,
    };
    let _ = protolink::tokio::server(CutIo { stream, state })
        .with_config(device_grpc::config())
        .build()
        .serve(&mut handler)
        .await;
}
