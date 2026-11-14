#![allow(dead_code)]
use ota_protocol::{StatusRequest, StatusResponse};
use std::{
    cell::RefCell,
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
    action: impl FnMut(StatusRequest) -> Result<Vec<u8>, tonic::Status> + Send + 'static,
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
    action: impl FnMut(ota_protocol::ReadLogsRequest) -> Result<Vec<u8>, tonic::Status> + Send + 'static,
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
    action: impl FnMut(M) -> Result<Vec<u8>, tonic::Status>,
) {
    let action = RefCell::new(action);
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
                let io = FragmentIo {
                    stream,
                    fragment,
                    delay,
                    sleep: None,
                };
                let _ = tokio::time::timeout(
                    Duration::from_secs(4),
                    embedded_http2::service::serve_streams(
                        embedded_io_adapters::tokio_1::FromTokio::new(io),
                        |request| exchange(request, &action),
                    ),
                )
                .await;
            }
        });
}
async fn exchange<M: ota_protocol::MessageDecode + Default>(
    request: embedded_http2::server::Request,
    action: &RefCell<impl FnMut(M) -> Result<Vec<u8>, tonic::Status>>,
) -> Result<(), embedded_http2::Error<std::io::Error>> {
    let field = |name: &str, value: &str| embedded_http2::HeaderField {
        name: name.into(),
        value: value.into(),
    };
    let embedded_http2::server::Request {
        mut body, respond, ..
    } = request;
    let result = async {
        let mut wire = Vec::new();
        while let Some(chunk) = body.data().await? {
            wire.extend_from_slice(chunk.as_bytes());
        }
        drop(body);
        let payload = ota_protocol::decode(&wire[5..]).unwrap();
        let mut send = respond.send_response(vec![
            field(":status", "200"),
            field("content-type", "application/grpc"),
        ])?;
        let reply = action.borrow_mut()(payload);
        let trailers = match reply {
            Ok(payload) => {
                let mut frame = vec![0];
                frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                frame.extend_from_slice(&payload);
                let mut remaining = frame.as_slice();
                while !remaining.is_empty() {
                    remaining = &remaining[send.send_data(remaining).await?..];
                }
                vec![field("grpc-status", "0")]
            }
            Err(status) => vec![
                field("grpc-status", &(status.code() as i32).to_string()),
                field("grpc-message", status.message()),
            ],
        };
        send.finish(trailers).await
    };
    let _ = result.await;
    Ok(())
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
