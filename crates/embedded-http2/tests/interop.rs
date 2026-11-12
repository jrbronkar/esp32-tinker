#[path = "support/protocol.rs"]
mod protocol;

use bytes::Bytes;
use embedded_http2::{Error, HeaderField, Response, Server};
use embedded_io_adapters::tokio_1::FromTokio;
use embedded_io_async::{ErrorType, Read, Write};
use http::Request;
use tokio::{
    net::{TcpListener, TcpStream},
    time::{Duration, timeout},
};

struct Fragmented(FromTokio<TcpStream>);

impl ErrorType for Fragmented {
    type Error = std::io::Error;
}

impl Read for Fragmented {
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        let limit = buffer.len().min(3);
        self.0.read(&mut buffer[..limit]).await
    }
}

impl Write for Fragmented {
    async fn write(&mut self, buffer: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buffer).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await
    }
}

fn field(name: &str, value: &str) -> HeaderField {
    HeaderField {
        name: name.into(),
        value: value.into(),
    }
}

#[tokio::test]
async fn h2_client_exchanges_headers_data_and_trailers_over_tcp() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut server = Server::handshake(Fragmented(FromTokio::new(socket)))
                .await
                .unwrap();

            for expected_id in [1, 3] {
                let request = server.accept().await.unwrap();
                assert_eq!(request.stream_id, expected_id);
                assert!(request.headers.contains(&field(":path", "/echo")));
                assert_eq!(request.body, b"hello");
                if expected_id == 1 {
                    assert_eq!(request.trailers, [field("x-request-end", "yes")]);
                } else {
                    assert!(request.trailers.is_empty());
                }
                server
                    .respond(
                        request.stream_id,
                        Response {
                            headers: &[
                                field(":status", "200"),
                                field("content-type", "text/plain"),
                            ],
                            body: &request.body,
                            trailers: &[field("x-result", "ok")],
                        },
                    )
                    .await
                    .unwrap();
            }
        });

        let socket = TcpStream::connect(addr).await.unwrap();
        // Force a WINDOW_UPDATE during each five-byte response.
        let (mut client, connection) = h2::client::Builder::new()
            .initial_window_size(3)
            .handshake::<_, Bytes>(socket)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);

        for with_trailers in [true, false] {
            client = client.ready().await.unwrap();
            let request = Request::post("http://localhost/echo").body(()).unwrap();
            let (response, mut send) = client.send_request(request, false).unwrap();
            send.send_data(Bytes::from_static(b"he"), false).unwrap();
            send.send_data(Bytes::from_static(b"llo"), !with_trailers)
                .unwrap();
            if with_trailers {
                let mut trailers = http::HeaderMap::new();
                trailers.insert("x-request-end", "yes".parse().unwrap());
                send.send_trailers(trailers).unwrap();
            }

            let response = response.await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "text/plain");
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = chunk.unwrap();
                body.flow_control().release_capacity(chunk.len()).unwrap();
                bytes.extend_from_slice(&chunk);
            }
            assert_eq!(bytes, b"hello");
            assert_eq!(body.trailers().await.unwrap().unwrap()["x-result"], "ok");
        }

        server.await.unwrap();
        driver.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn h2_client_reset_interrupts_a_blocked_response() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut server = Server::handshake(FromTokio::new(socket)).await.unwrap();
            let request = server.accept().await.unwrap();
            assert_eq!(request.stream_id, 1);
            let result = server
                .respond(
                    request.stream_id,
                    Response {
                        headers: &[field(":status", "200")],
                        body: b"blocked by the client's zero receive window",
                        trailers: &[],
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(Error::Reset {
                    stream_id: 1,
                    code: 8
                })
            ));
            let request = server.accept().await.unwrap();
            assert_eq!(request.stream_id, 3);
            server
                .respond(
                    request.stream_id,
                    Response {
                        headers: &[field(":status", "204")],
                        body: &[],
                        trailers: &[],
                    },
                )
                .await
                .unwrap();
        });

        let socket = TcpStream::connect(addr).await.unwrap();
        let (mut client, connection) = h2::client::Builder::new()
            .initial_window_size(0)
            .handshake::<_, Bytes>(socket)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        client = client.ready().await.unwrap();
        let request = Request::get("http://localhost/reset").body(()).unwrap();
        let (response, mut send) = client.send_request(request, true).unwrap();
        let response = response.await.unwrap();
        assert_eq!(response.status(), 200);
        send.send_reset(h2::Reason::CANCEL);

        client = client.ready().await.unwrap();
        let request = Request::get("http://localhost/next").body(()).unwrap();
        let (response, _) = client.send_request(request, true).unwrap();
        assert_eq!(response.await.unwrap().status(), 204);
        server.await.unwrap();
        driver.abort();
    })
    .await
    .unwrap();
}
