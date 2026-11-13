use std::time::Duration;

use bytes::Bytes;
use embedded_grpc::{Service, Status, UnaryRequest, UnaryResponse, serve};
use embedded_io_adapters::tokio_1::FromTokio;
use http::{Method, Request, StatusCode};
use ota_protocol::StatusRequest;
use tokio::io::{AsyncWriteExt, duplex};

const PATH: &str = "/test.Echo/Call";
const OVERFLOW_PATH: &str = "/test.Echo/Overflow";
const EMPTY_MESSAGE: &[u8] = &[0, 0, 0, 0, 0];
const VALID_MESSAGE: &[u8] = &[0, 0, 0, 0, 2, 8, 1];

#[derive(Default)]
struct Echo {
    decoded: usize,
}

impl Service for Echo {
    async fn call(
        &mut self,
        path: &str,
        request: UnaryRequest<'_>,
    ) -> Result<UnaryResponse, Status> {
        let message = request.decode::<StatusRequest, 2>()?;
        self.decoded += 1;
        match path {
            PATH => UnaryResponse::encode::<_, 2>(message),
            OVERFLOW_PATH => UnaryResponse::encode::<_, 1>(message),
            _ => Err(Status::Unimplemented),
        }
    }
}

fn request(method: Method, path: &str, content_type: &str) -> Request<()> {
    Request::builder()
        .method(method)
        .uri(format!("http://localhost{path}"))
        .header("content-type", content_type)
        .body(())
        .unwrap()
}

async fn send(
    client: &mut h2::client::SendRequest<Bytes>,
    request: Request<()>,
    body: &'static [u8],
) -> http::Response<h2::RecvStream> {
    std::future::poll_fn(|cx| client.poll_ready(cx))
        .await
        .unwrap();
    let (reply, mut stream) = client.send_request(request, body.is_empty()).unwrap();
    if !body.is_empty() {
        stream.send_data(Bytes::from_static(body), true).unwrap();
    }
    reply.await.unwrap()
}

async fn assert_grpc_reply(
    response: http::Response<h2::RecvStream>,
    status: &str,
    expected_data: &[u8],
) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/grpc");
    let mut body = response.into_body();
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await {
        data.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(data, expected_data);
    assert_eq!(
        body.trailers().await.unwrap().unwrap()["grpc-status"],
        status
    );
}

#[tokio::test]
async fn rejects_invalid_requests_and_keeps_the_connection_usable() {
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let (peer, socket) = duplex(8192);
                let server = tokio::task::spawn_local(async move {
                    let mut service = Echo::default();
                    let error = serve(FromTokio::new(socket), &mut service)
                        .await
                        .unwrap_err();
                    assert!(error.is_disconnected(), "{error:?}");
                    service.decoded
                });
                let (mut client, connection) = h2::client::handshake(peer).await.unwrap();
                let driver = tokio::spawn(connection);

                let invalid_type = request(Method::POST, PATH, "application/grpc-web");
                let response = send(&mut client, invalid_type, EMPTY_MESSAGE).await;
                assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
                assert!(response.into_body().data().await.is_none());

                let invalid_method = request(Method::GET, PATH, "application/grpc");
                let response = send(&mut client, invalid_method, EMPTY_MESSAGE).await;
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
                assert_eq!(response.headers()["allow"], "POST");
                assert!(response.into_body().data().await.is_none());

                let mut compressed = request(Method::POST, PATH, "application/grpc");
                compressed
                    .headers_mut()
                    .insert("grpc-encoding", "gzip".parse().unwrap());
                let response = send(&mut client, compressed, EMPTY_MESSAGE).await;
                assert_eq!(response.headers()["grpc-accept-encoding"], "identity");
                assert_grpc_reply(response, "12", &[]).await;

                for (name, body, status) in [
                    ("empty body", &[][..], "3"),
                    ("short header", &[0, 0, 0, 0], "3"),
                    ("compressed envelope", &[1, 0, 0, 0, 0], "12"),
                    ("invalid compression flag", &[2, 0, 0, 0, 0], "3"),
                    ("oversized payload", &[0, 0, 0, 0, 3, 8, 1, 0], "8"),
                    ("truncated payload", &[0, 0, 0, 0, 1], "3"),
                    ("extra envelope", &[0; 10], "3"),
                    ("malformed protobuf", &[0, 0, 0, 0, 1, 0xff], "3"),
                ] {
                    let response = send(
                        &mut client,
                        request(Method::POST, PATH, "application/grpc"),
                        body,
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::OK, "{name}");
                    assert_grpc_reply(response, status, &[]).await;
                }

                let response = send(
                    &mut client,
                    request(Method::POST, OVERFLOW_PATH, "application/grpc+proto"),
                    VALID_MESSAGE,
                )
                .await;
                assert_grpc_reply(response, "13", &[]).await;

                let mut identity = request(Method::POST, PATH, "application/grpc; charset=utf-8");
                identity
                    .headers_mut()
                    .insert("grpc-encoding", "identity".parse().unwrap());
                let response = send(&mut client, identity, VALID_MESSAGE).await;
                assert_grpc_reply(response, "0", VALID_MESSAGE).await;

                drop(client);
                driver.abort();
                assert_eq!(server.await.unwrap(), 2);
            })
            .await
            .expect("gRPC exchanges timed out");
        })
        .await;
}

#[tokio::test]
async fn invalid_preface_is_reported_as_a_connection_failure() {
    let (mut peer, socket) = duplex(64);
    peer.write_all(&[b'x'; 24]).await.unwrap();
    let mut service = Echo::default();
    let error = serve(FromTokio::new(socket), &mut service)
        .await
        .unwrap_err();
    assert!(!error.is_disconnected());
    assert_eq!(service.decoded, 0);
}

#[tokio::test]
async fn stalled_response_stops_polling_and_reset_drops_the_producer() {
    use embedded_grpc::{Stream, StreamError, StreamingCall, StreamingService, serve_streaming};
    use std::{
        cell::Cell,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Default)]
    struct ProducerState {
        polls: Cell<usize>,
        dropped: Cell<bool>,
    }

    struct Producer<'a>(&'a ProducerState);

    impl Stream for Producer<'_> {
        type Item = Result<StatusRequest, Status>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.polls.set(self.0.polls.get() + 1);
            Poll::Ready(Some(Ok(StatusRequest::default())))
        }
    }

    impl Drop for Producer<'_> {
        fn drop(&mut self) {
            self.0.dropped.set(true);
        }
    }

    impl StreamingService for ProducerState {
        async fn call(&self, path: &str, call: StreamingCall) -> Result<(), StreamError> {
            let Some((request, response)) = call.unary::<StatusRequest, 2>().await? else {
                return Ok(());
            };
            if path == PATH {
                response.stream::<_, 2>(Producer(self)).await
            } else {
                response.reply::<_, 2>(request).await
            }
        }
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        let (peer, socket) = duplex(8192);
        let service = ProducerState::default();
        let client = async {
            let (mut client, driver) = h2::client::Builder::new()
                .initial_window_size(1)
                .handshake(peer)
                .await
                .unwrap();
            let driver = tokio::spawn(driver);
            std::future::poll_fn(|cx| client.poll_ready(cx))
                .await
                .unwrap();
            let (reply, mut request_body) = client
                .send_request(request(Method::POST, PATH, "application/grpc"), false)
                .unwrap();
            request_body
                .send_data(Bytes::from_static(EMPTY_MESSAGE), true)
                .unwrap();
            let mut body = reply.await.unwrap().into_body();
            assert_eq!(body.data().await.unwrap().unwrap().len(), 1);
            assert_eq!(service.polls.get(), 1);

            let mut sibling = send(
                &mut client,
                request(Method::POST, "/test.Echo/Unary", "application/grpc"),
                EMPTY_MESSAGE,
            )
            .await
            .into_body();
            let mut received = Vec::new();
            while let Some(chunk) = sibling.data().await {
                let chunk = chunk.unwrap();
                sibling
                    .flow_control()
                    .release_capacity(chunk.len())
                    .unwrap();
                received.extend_from_slice(&chunk);
            }
            assert_eq!(received, EMPTY_MESSAGE);
            assert_eq!(
                sibling.trailers().await.unwrap().unwrap()["grpc-status"],
                "0"
            );
            assert_eq!(service.polls.get(), 1);

            request_body.send_reset(h2::Reason::CANCEL);
            while !service.dropped.get() {
                tokio::task::yield_now().await;
            }
            assert_eq!(service.polls.get(), 1);
            driver.abort();
        };
        let (server, ()) = tokio::join!(serve_streaming(FromTokio::new(socket), &service), client);
        assert!(server.unwrap_err().is_disconnected());
    })
    .await
    .expect("streaming cancellation timed out");
}
