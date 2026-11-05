#![allow(dead_code)]
use ota_protocol::{StatusRequest, StatusResponse};
use std::{
    io,
    net::{SocketAddr, TcpListener},
    pin::Pin,
    task::{Context, Poll},
    thread,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
pub fn bytes(response: &impl ota_protocol::MessageEncode) -> Vec<u8> {
    ota_protocol::encode(response).unwrap().to_vec()
}
pub fn reply(_: StatusRequest) -> StatusResponse {
    ota_protocol::status("mock-board", "device-demo", "test-v1", 1234).unwrap()
}
pub fn spawn(
    connections: usize,
    fragment: usize,
    delay: Duration,
    action: impl FnMut(StatusRequest) -> Result<Vec<u8>, protolink::Status> + Send + 'static,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || run(listener, connections, fragment, delay, action));
    (address, handle)
}
pub fn spawn_logs(
    connections: usize,
    fragment: usize,
    delay: Duration,
    action: impl FnMut(ota_protocol::ReadLogsRequest) -> Result<Vec<u8>, protolink::Status>
        + Send
        + 'static,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || run(listener, connections, fragment, delay, action));
    (address, handle)
}
pub fn run<M: ota_protocol::MessageDecode + Default>(
    listener: TcpListener,
    connections: usize,
    fragment: usize,
    delay: Duration,
    mut action: impl FnMut(M) -> Result<Vec<u8>, protolink::Status>,
) {
    listener.set_nonblocking(true).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            for _ in 0..connections {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(4), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let mut handler = TestHandler {
                    reply: |body: &[u8]| action(ota_protocol::decode(body).unwrap()),
                    pending: None,
                };
                let io = FragmentIo {
                    stream,
                    fragment,
                    delay,
                    sleep: None,
                };
                let _ = tokio::time::timeout(
                    Duration::from_secs(4),
                    protolink::tokio::server(io)
                        .with_config(device_grpc::config())
                        .build()
                        .serve(&mut handler),
                )
                .await;
            }
        });
}
struct TestHandler<F> {
    reply: F,
    pending: Option<Vec<u8>>,
}
impl<F: FnMut(&[u8]) -> Result<Vec<u8>, protolink::Status>> protolink::Handler for TestHandler<F> {
    fn route(&self, path: &str) -> Option<protolink::Route> {
        let kind = match path {
            device_grpc::STATUS_PATH => protolink::MethodKind::Unary,
            device_grpc::LOGS_PATH => protolink::MethodKind::ServerStreaming,
            _ => return None,
        };
        Some(protolink::Route::new(kind, 0))
    }
    fn call(
        &mut self,
        _: &mut protolink::CallContext<'_>,
        request: &[u8],
    ) -> Result<Vec<u8>, protolink::Status> {
        (self.reply)(request)
    }
    fn on_message(
        &mut self,
        _: &mut protolink::CallContext<'_>,
        request: &[u8],
    ) -> Result<(), protolink::Status> {
        self.pending = Some((self.reply)(request)?);
        Ok(())
    }
    fn poll_response(
        &mut self,
        _: &mut protolink::CallContext<'_>,
        _: &mut Context<'_>,
    ) -> Poll<protolink::Next<Vec<u8>>> {
        Poll::Ready(match self.pending.take() {
            Some(bytes) => protolink::Next::Message(bytes),
            None => protolink::Next::Done(Ok(())),
        })
    }
}

struct FragmentIo {
    stream: tokio::net::TcpStream,
    fragment: usize,
    delay: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl AsyncRead for FragmentIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for FragmentIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::future::Future;
        if let Some(sleep) = self.sleep.as_mut() {
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.sleep = None;
        }
        let n = buf.len().min(self.fragment);
        match Pin::new(&mut self.stream).poll_write(cx, &buf[..n]) {
            Poll::Ready(Ok(n)) => {
                if !self.delay.is_zero() {
                    self.sleep = Some(Box::pin(tokio::time::sleep(self.delay)));
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
