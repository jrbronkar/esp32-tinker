#[path = "support/protocol.rs"]
mod protocol;

use bytes::Bytes;
use embedded_http2::{BodyFrame, Error, HeaderField, Response, SendEvent, Server};
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
        let (reset_seen, reset_received) = tokio::sync::oneshot::channel();
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
            reset_seen.send(()).unwrap();
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
        reset_received.await.unwrap();

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

#[tokio::test]
async fn h2_client_receives_ack_before_request_closes() -> Result<(), Box<dyn std::error::Error>> {
    const FIRST_OPERATION: &[u8] = b"first";
    const NEXT_OPERATION: &[u8] = b"next";
    const ACK_PREFIX: &[u8] = b"ac";
    const ACK_SUFFIX: &[u8] = b"k!";
    const ACK: &[u8] = b"ack!";
    const GRPC_OK: &str = "0";
    const DUPLEX_BUFFER: usize = 4096;

    timeout(Duration::from_secs(5), async {
        let (peer, socket) = tokio::io::duplex(DUPLEX_BUFFER);
        let (observed_body, body_observed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut server = Server::handshake(FromTokio::new(socket))
                .await
                .map_err(transport_error)?;
            let head = server.accept_head().await.map_err(transport_error)?;
            let id = head.stream_id;
            assert!(!head.end_stream);
            assert!(matches!(
                server.read_body(id).await.map_err(transport_error)?,
                Some(BodyFrame::Data(bytes)) if bytes == FIRST_OPERATION
            ));
            server.release_capacity(id).await.map_err(transport_error)?;
            server
                .start_response(
                    id,
                    &[
                        field(":status", "200"),
                        field("content-type", "application/grpc"),
                    ],
                )
                .await
                .map_err(transport_error)?;
            assert_eq!(
                server.send_data(id, ACK).await.map_err(transport_error)?,
                ACK_PREFIX.len()
            );
            assert_eq!(
                server
                    .send_data(id, ACK_SUFFIX)
                    .await
                    .map_err(transport_error)?,
                0
            );
            assert!(matches!(
                server.wait_send_capacity(id).await.map_err(transport_error)?,
                SendEvent::RequestBody(BodyFrame::Data(bytes)) if bytes == NEXT_OPERATION
            ));
            server.release_capacity(id).await.map_err(transport_error)?;
            observed_body
                .send(())
                .map_err(|()| "client stopped waiting")?;
            assert!(matches!(
                server
                    .wait_send_capacity(id)
                    .await
                    .map_err(transport_error)?,
                SendEvent::Ready
            ));
            assert_eq!(
                server
                    .send_data(id, ACK_SUFFIX)
                    .await
                    .map_err(transport_error)?,
                ACK_SUFFIX.len()
            );

            assert!(matches!(
                server.read_body(id).await.map_err(transport_error)?,
                Some(BodyFrame::Trailers(fields)) if fields == [field("x-upload-end", "yes")]
            ));
            assert!(
                server
                    .read_body(id)
                    .await
                    .map_err(transport_error)?
                    .is_none()
            );
            server
                .finish_response(id, &[field("grpc-status", GRPC_OK)])
                .await
                .map_err(transport_error)
        });

        let (mut client, connection) = h2::client::Builder::new()
            .initial_window_size(ACK_PREFIX.len() as u32)
            .handshake::<_, Bytes>(peer)
            .await?;
        let driver = tokio::spawn(connection);
        let request = Request::post("http://localhost/test.Upload/Upload").body(())?;
        let (reply, mut send) = client.send_request(request, false)?;
        send.send_data(Bytes::from_static(FIRST_OPERATION), false)?;

        let reply = reply.await?;
        assert_eq!(reply.status(), http::StatusCode::OK);
        let mut body = reply.into_body();
        let prefix = body.data().await.ok_or("missing ACK prefix")??;
        assert_eq!(prefix.as_ref(), ACK_PREFIX);

        // The server must receive this frame even while its send window is full.
        send.send_data(Bytes::from_static(NEXT_OPERATION), false)?;
        body_observed.await?;
        body.flow_control().release_capacity(prefix.len())?;
        let suffix = body.data().await.ok_or("missing ACK suffix")??;
        assert_eq!(suffix.as_ref(), ACK_SUFFIX);

        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-upload-end", http::HeaderValue::from_static("yes"));
        send.send_trailers(trailers)?;
        assert!(body.data().await.is_none());
        let trailers = body.trailers().await?.ok_or("missing gRPC status")?;
        assert_eq!(trailers["grpc-status"], GRPC_OK);
        server.await??;
        driver.abort();
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .await?
}

fn transport_error(error: Error<std::io::Error>) -> String {
    format!("{error:?}")
}

#[tokio::test]
async fn stalled_stream_leaves_another_stream_free_to_progress()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            use embedded_http2::Event;

            let (socket, _) = listener.accept().await.map_err(|error| error.to_string())?;
            let mut server = Server::handshake(FromTokio::new(socket))
                .await
                .map_err(|error| format!("{error:?}"))?;
            let mut stalled = None;
            loop {
                match server
                    .next_event()
                    .await
                    .map_err(|error| format!("{error:?}"))?
                {
                    Event::Request(head) if stalled.is_none() => {
                        stalled = Some(head.stream_id);
                        server
                            .start_response(head.stream_id, &[field(":status", "200")])
                            .await
                            .map_err(|error| format!("{error:?}"))?;
                        assert_eq!(
                            server
                                .send_data(head.stream_id, b"ab")
                                .await
                                .map_err(|error| format!("{error:?}"))?,
                            1
                        );
                        assert_eq!(
                            server
                                .send_data(head.stream_id, b"b")
                                .await
                                .map_err(|error| format!("{error:?}"))?,
                            0
                        );
                    }
                    Event::Request(head) => {
                        assert!(head.end_stream);
                        server
                            .start_response(head.stream_id, &[field(":status", "200")])
                            .await
                            .map_err(|error| format!("{error:?}"))?;
                        assert_eq!(
                            server
                                .send_data(head.stream_id, b"z")
                                .await
                                .map_err(|error| format!("{error:?}"))?,
                            1
                        );
                        server
                            .finish_response(head.stream_id, &[field("x-result", "ok")])
                            .await
                            .map_err(|error| format!("{error:?}"))?;
                    }
                    Event::Reset { stream_id, code } => {
                        assert_eq!(Some(stream_id), stalled);
                        assert_eq!(code, 8);
                        return Ok::<_, String>(());
                    }
                    _ => {}
                }
            }
        });

        let socket = TcpStream::connect(addr).await?;
        let (client, connection) = h2::client::Builder::new()
            .initial_window_size(1)
            .handshake::<_, Bytes>(socket)
            .await?;
        let driver = tokio::spawn(connection);
        let mut client = client.ready().await?;
        let (first, mut first_send) =
            client.send_request(Request::get("http://localhost/first").body(())?, true)?;
        let mut first = first.await?.into_body();
        assert_eq!(
            first.data().await.transpose()?,
            Some(Bytes::from_static(b"a"))
        );

        let mut client = client.ready().await?;
        let (second, _) =
            client.send_request(Request::get("http://localhost/second").body(())?, true)?;
        let mut second = second.await?.into_body();
        assert_eq!(
            second.data().await.transpose()?,
            Some(Bytes::from_static(b"z"))
        );
        assert!(second.data().await.transpose()?.is_none());
        assert_eq!(
            second
                .trailers()
                .await?
                .and_then(|trailers| trailers.get("x-result").cloned()),
            Some(http::HeaderValue::from_static("ok"))
        );
        first_send.send_reset(h2::Reason::CANCEL);
        server.await??;
        driver.abort();
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

#[tokio::test]
async fn buffered_service_cancels_a_pending_handler_and_releases_the_next_call() {
    use embedded_http2::service;
    use std::future::pending;
    use tokio::{io::duplex, sync::oneshot};

    struct Handler {
        started: Option<oneshot::Sender<()>>,
        canceled: Option<oneshot::Sender<()>>,
    }

    struct CancelOnDrop(Option<oneshot::Sender<()>>);

    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    impl service::Service for Handler {
        async fn call(&mut self, request: service::Request) -> service::Response {
            if request.headers.contains(&field(":path", "/slow")) {
                let _guard = CancelOnDrop(self.canceled.take());
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                pending::<()>().await;
            }
            service::Response {
                headers: vec![field(":status", "204")],
                body: Vec::new(),
                trailers: Vec::new(),
            }
        }
    }

    timeout(Duration::from_secs(5), async {
        let (socket, peer) = duplex(8192);
        let (started, running) = oneshot::channel();
        let (canceled, dropped) = oneshot::channel();
        let mut handler = Handler {
            started: Some(started),
            canceled: Some(canceled),
        };
        let client = async {
            let (mut client, connection) = h2::client::handshake(peer).await.unwrap();
            let driver = tokio::spawn(connection);
            let request = |path| {
                Request::builder()
                    .uri(format!("http://localhost{path}"))
                    .body(())
                    .unwrap()
            };
            client = client.ready().await.unwrap();
            let (_response, mut slow) = client.send_request(request("/slow"), true).unwrap();
            running.await.unwrap();
            client = client.ready().await.unwrap();
            let (next, _) = client.send_request(request("/next"), true).unwrap();
            slow.send_reset(h2::Reason::CANCEL);
            dropped.await.unwrap();
            let next = next.await.unwrap();
            assert_eq!(next.status(), http::StatusCode::NO_CONTENT);
            assert!(next.body().is_end_stream());
            driver.abort();
        };
        let (server, ()) =
            tokio::join!(service::serve(FromTokio::new(socket), &mut handler), client);
        assert!(matches!(
            server,
            Err(Error::Read(
                embedded_io_async::ReadExactError::UnexpectedEof
            ))
        ));
    })
    .await
    .expect("buffered cancellation timed out");
}

#[tokio::test]
async fn buffered_service_reports_oversized_responses() {
    use embedded_http2::service;

    struct Oversized;

    impl service::Service for Oversized {
        async fn call(&mut self, _: service::Request) -> service::Response {
            service::Response {
                headers: vec![field(":status", "200")],
                body: vec![0; 16 * 1024 + 1],
                trailers: Vec::new(),
            }
        }
    }

    timeout(Duration::from_secs(5), async {
        let (socket, peer) = tokio::io::duplex(8192);
        let mut handler = Oversized;
        let client = async {
            let (client, connection) = h2::client::handshake(peer).await.unwrap();
            let driver = tokio::spawn(connection);
            let mut client = client.ready().await.unwrap();
            let request = Request::builder()
                .uri("http://localhost/")
                .body(())
                .unwrap();
            let (reply, _) = client.send_request(request, true).unwrap();
            assert!(reply.await.is_err());
            driver.abort();
        };
        let (server, ()) =
            tokio::join!(service::serve(FromTokio::new(socket), &mut handler), client);
        assert!(matches!(server, Err(Error::BodyLimit)));
    })
    .await
    .expect("buffered response limit timed out");
}
