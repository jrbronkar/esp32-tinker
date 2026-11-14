//! Loopback gRPC board with the firmware management handler and RAM-backed flash.
use embedded_grpc::{Status, Stream, Streaming};
use ota_protocol::{UploadAck, UploadOperation as Op, UploadPhase, UploadRequest, UPLOAD_FRAME};
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
    target: &'a RefCell<Memory>,
    env: Env,
}

impl embedded_grpc::StreamingService for Faulty<'_> {
    async fn call(
        &self,
        path: &str,
        call: embedded_grpc::StreamingCall<'_>,
    ) -> Result<(), embedded_grpc::StreamError> {
        if path != ota_protocol::UPLOAD_PATH {
            let handler = management_grpc::ManagementServer::new(self.management());
            return handler.call(path, call).await;
        }
        let (request, response) = call.streaming();
        response
            .stream::<_, UPLOAD_FRAME>(FaultReplies {
                inbound: request.into_inner(),
                session: device_grpc::UploadSession::new(self.target, 0x310000, self.env.clone()),
                state: &self.env.0,
                ended: false,
            })
            .await
    }
}

impl Faulty<'_> {
    fn management(&self) -> device_grpc::EmbeddedManagement<'_, Memory, Env> {
        device_grpc::EmbeddedManagement::new(self.target, 0x310000, self.env.clone())
    }
}

enum WireAck {
    Typed(UploadAck),
    Malformed,
}

impl micropb::MessageEncode for WireAck {
    const MAX_SIZE: Result<usize, &'static str> = UploadAck::MAX_SIZE;

    fn encode<W: micropb::PbWrite>(
        &self,
        encoder: &mut micropb::PbEncoder<W>,
    ) -> Result<(), W::Error> {
        match self {
            Self::Typed(ack) => ack.encode(encoder),
            Self::Malformed => encoder.encode_varint32(255),
        }
    }

    fn compute_size(&self) -> usize {
        match self {
            Self::Typed(ack) => ack.compute_size(),
            Self::Malformed => 2,
        }
    }
}

struct FaultReplies<'a> {
    inbound: Streaming<UploadRequest, UPLOAD_FRAME>,
    session: device_grpc::UploadSession<'a, Memory, Env>,
    state: &'a State,
    ended: bool,
}

impl Stream for FaultReplies<'_> {
    type Item = Result<WireAck, Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let Some(request) = std::task::ready!(std::pin::Pin::new(&mut self.inbound).poll_next(cx))
        else {
            return Poll::Ready(None);
        };
        Poll::Ready(Some(request.and_then(|request| self.reply(request))))
    }
}

impl FaultReplies<'_> {
    fn reply(&mut self, request: UploadRequest) -> Result<WireAck, Status> {
        let name = match request.payload {
            Some(Op::Begin(_)) => "begin",
            Some(Op::Chunk(_)) => "chunk",
            Some(Op::Finish(_)) => "finish",
            Some(Op::Commit(_)) => "commit",
            Some(Op::Abort(_)) => "abort",
            _ => "other",
        };
        self.state.requests.borrow_mut().push(name);
        let mut ack = self.session.process(request)?;
        self.ended = matches!(ack.phase, UploadPhase::Aborted | UploadPhase::Committed);
        match self.state.fault {
            "lost-commit" if ack.phase == UploadPhase::Committed => {
                return Err(Status::Unavailable)
            }
            "disconnect" => return Err(Status::Unavailable),
            "truncated" => return Ok(WireAck::Malformed),
            "id" => ack.request_id += 1,
            "phase" => ack.phase = UploadPhase::Verified,
            "offset" => ack.next_offset += 1,
            "target" => ack.target_offset = 0x9000,
            _ => {}
        }
        Ok(WireAck::Typed(ack))
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
    let handler = Faulty {
        target: &target,
        env: Env(state.clone()),
    };
    let _ = embedded_grpc::serve_streaming(
        embedded_io_adapters::tokio_1::FromTokio::new(CutIo { stream, state }),
        &handler,
    )
    .await;
}
