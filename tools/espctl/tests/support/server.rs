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
                let mut handler = protolink::grpc::FnHandler(|path: &str, body: &[u8]| {
                    if path != device_grpc::STATUS_PATH && path != device_grpc::READ_LOGS_PATH {
                        return Some(Err(protolink::Status::unimplemented("unknown path")));
                    }
                    let request = ota_protocol::decode(body).unwrap();
                    Some(action(request))
                });
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
