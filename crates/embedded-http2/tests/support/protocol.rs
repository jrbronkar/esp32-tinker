use embedded_http2::{Error, HeaderField, Response, Server};
use embedded_io_adapters::tokio_1::FromTokio;
use tokio::io::{AsyncWriteExt, DuplexStream, duplex};
use zerodds_http2::{
    CLIENT_PREFACE, ErrorCode, Flags, FrameHeader, FrameType, Http2Error, encode_frame,
    flow::encode_window_update,
    frame::{DEFAULT_MAX_FRAME_SIZE, FRAME_HEADER_LEN},
    settings::{Setting, SettingId, encode_settings},
};

type TestServer = Server<FromTokio<DuplexStream>>;
type TestError = Error<std::io::Error>;

const END_HEADERS: u8 = Flags::END_HEADERS;
const END_STREAM: u8 = Flags::END_STREAM;
const REQUEST: &[u8] = &[0x82, 0x86, 0x84]; // GET, http, /

fn frame(kind: FrameType, stream_id: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    let header = FrameHeader {
        length: payload.len() as u32,
        frame_type: kind,
        flags: Flags(flags),
        stream_id,
    };
    let mut bytes = vec![0; FRAME_HEADER_LEN + payload.len()];
    encode_frame(&header, payload, &mut bytes, DEFAULT_MAX_FRAME_SIZE).unwrap();
    bytes
}

async fn open(bytes: &[u8]) -> Result<(TestServer, DuplexStream), TestError> {
    let (socket, mut peer) = duplex(128 * 1024);
    peer.write_all(bytes).await.unwrap();
    peer.shutdown().await.unwrap();
    let server = Server::handshake(FromTokio::new(socket)).await?;
    Ok((server, peer))
}

async fn connection(frames: &[Vec<u8>]) -> (TestServer, DuplexStream) {
    let mut input = CLIENT_PREFACE.to_vec();
    input.extend(frame(FrameType::Settings, 0, 0, &[]));
    for frame in frames {
        input.extend(frame);
    }
    open(&input).await.unwrap()
}

#[tokio::test]
async fn handshake_requires_the_preface_and_initial_settings() {
    let mut preface = CLIENT_PREFACE.to_vec();
    preface[0] = b'X';
    assert!(matches!(
        open(&preface).await,
        Err(Error::Wire(Http2Error::BadPreface))
    ));
    assert!(matches!(open(&preface[..3]).await, Err(Error::Read(_))));

    let mut input = CLIENT_PREFACE.to_vec();
    input.extend(frame(FrameType::Settings, 0, Flags::ACK, &[]));
    assert!(matches!(open(&input).await, Err(Error::Protocol)));

    for invalid in [
        frame(FrameType::Settings, 0, 0, &[0]),
        frame(
            FrameType::Settings,
            0,
            0,
            &encode_settings(&[Setting {
                id: SettingId::EnablePush,
                value: 2,
            }]),
        ),
    ] {
        let mut input = CLIENT_PREFACE.to_vec();
        input.extend(invalid);
        assert!(matches!(open(&input).await, Err(Error::Wire(_))));
    }

    let (socket, mut peer) = duplex(128);
    peer.write_all(CLIENT_PREFACE).await.unwrap();
    drop(peer);
    assert!(matches!(
        Server::handshake(FromTokio::new(socket)).await,
        Err(Error::Write(_))
    ));
}

#[tokio::test]
async fn rejects_invalid_connection_and_stream_frames() {
    let mut oversized = frame(FrameType::Data, 1, 0, &[]);
    oversized[..3].copy_from_slice(&[0, 0x40, 1]);
    let (mut server, _peer) = connection(&[oversized]).await;
    assert!(matches!(
        server.accept().await,
        Err(Error::Wire(Http2Error::FrameTooLarge { .. }))
    ));

    for invalid in [
        frame(FrameType::Settings, 0, Flags::ACK, &[0; 6]),
        frame(FrameType::Headers, 2, END_HEADERS | END_STREAM, REQUEST),
        frame(FrameType::WindowUpdate, 1, 0, &encode_window_update(1)),
        frame(FrameType::RstStream, 1, 0, &8_u32.to_be_bytes()),
    ] {
        let (mut server, _peer) = connection(&[invalid]).await;
        assert!(matches!(server.accept().await, Err(Error::Protocol)));
    }

    let (mut server, _peer) = connection(&[frame(FrameType::Ping, 1, 0, &[0; 8])]).await;
    assert!(matches!(
        server.accept().await,
        Err(Error::Wire(Http2Error::Protocol(ErrorCode::ProtocolError)))
    ));

    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
        frame(FrameType::Data, 3, END_STREAM, b"wrong stream"),
    ])
    .await;
    assert!(matches!(server.accept().await, Err(Error::Protocol)));
}

#[tokio::test]
async fn rejects_truncated_frames_and_invalid_padding() {
    let mut truncated = frame(FrameType::Data, 1, END_STREAM, b"body");
    truncated.pop();
    for (input, truncated) in [
        (truncated, true),
        (
            frame(FrameType::Data, 1, Flags::PADDED | END_STREAM, &[2, b'a']),
            false,
        ),
    ] {
        let (mut server, _peer) =
            connection(&[frame(FrameType::Headers, 1, END_HEADERS, REQUEST), input]).await;
        let error = server.accept().await.unwrap_err();
        assert!(if truncated {
            matches!(error, Error::Read(_))
        } else {
            matches!(error, Error::Protocol)
        });
    }
}

#[tokio::test]
async fn assembles_continuations_and_skips_extension_frames() {
    let mut extension = frame(FrameType::Data, 0, 0, b"extension");
    extension[3] = 0xfe;
    let (mut server, _peer) = connection(&[
        extension,
        frame(FrameType::Priority, 1, 0, &[0; 5]),
        frame(FrameType::Ping, 0, 0, b"12345678"),
        frame(FrameType::Ping, 0, Flags::ACK, b"12345678"),
        frame(FrameType::WindowUpdate, 0, 0, &encode_window_update(10)),
        frame(
            FrameType::Headers,
            1,
            Flags::PRIORITY | Flags::PADDED,
            &[1, 0, 0, 0, 0, 0, 0x20, 0x82, 0],
        ),
        frame(
            FrameType::Continuation,
            1,
            END_HEADERS,
            &[0x86, 0x84, 0, 1, b'x', 1, b'y'],
        ),
        frame(
            FrameType::Settings,
            0,
            0,
            &encode_settings(&[Setting {
                id: SettingId::InitialWindowSize,
                value: 1024,
            }]),
        ),
        frame(FrameType::Data, 1, 0, &[]),
        frame(
            FrameType::Data,
            1,
            Flags::PADDED | END_STREAM,
            &[1, b'a', 0],
        ),
    ])
    .await;
    let request = server.accept().await.unwrap();
    assert_eq!(request.body, b"a");
    assert_eq!(request.headers.len(), 4);
    assert_eq!(request.headers[3].name, "x");
    assert_eq!(request.headers[3].value, "y");
    let headers = [HeaderField {
        name: ":status".into(),
        value: "200".into(),
    }];
    server
        .respond(
            request.stream_id,
            Response {
                headers: &headers,
                body: b"ok",
                trailers: &[],
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn rejects_invalid_continuations_and_header_limits() {
    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, 0, REQUEST),
        frame(FrameType::Ping, 0, 0, &[0; 8]),
    ])
    .await;
    assert!(matches!(server.accept().await, Err(Error::Protocol)));

    for block in [
        vec![0x82; 4097],          // Encoded block limit.
        vec![0x82; 150],           // Decoded header list limit.
        vec![0x82, 0x20],          // HPACK table updates must precede fields.
        vec![0xff],                // Truncated indexed field.
        vec![0x80],                // Index zero is invalid.
        vec![0, 1, b'x', 2, b'y'], // Truncated literal value.
    ] {
        let (mut server, _peer) = connection(&[frame(
            FrameType::Headers,
            1,
            END_HEADERS | END_STREAM,
            &block,
        )])
        .await;
        assert!(matches!(server.accept().await, Err(Error::HeaderBlock)));
    }

    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, END_STREAM, &vec![0x82; 4096]),
        frame(FrameType::Continuation, 1, END_HEADERS, &[0x82]),
    ])
    .await;
    assert!(matches!(server.accept().await, Err(Error::HeaderBlock)));
}

#[tokio::test]
async fn rejects_oversized_request_and_response_bodies() {
    let body = vec![0; DEFAULT_MAX_FRAME_SIZE as usize];
    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
        frame(FrameType::Data, 1, 0, &body),
        frame(FrameType::Data, 1, END_STREAM, &[0]),
    ])
    .await;
    assert!(matches!(server.accept().await, Err(Error::BodyLimit)));

    let (mut server, _peer) = connection(&[frame(
        FrameType::Headers,
        1,
        END_HEADERS | END_STREAM,
        REQUEST,
    )])
    .await;
    let request = server.accept().await.unwrap();
    let response = Response {
        headers: &[],
        body: &vec![0; body.len() + 1],
        trailers: &[],
    };
    assert!(matches!(
        server.respond(request.stream_id, response).await,
        Err(Error::BodyLimit)
    ));
}

#[tokio::test]
async fn enforces_response_order_and_header_limit() {
    let request_frame = frame(FrameType::Headers, 1, END_HEADERS | END_STREAM, REQUEST);
    let (mut server, _peer) = connection(core::slice::from_ref(&request_frame)).await;
    let request = server.accept().await.unwrap();
    assert!(matches!(server.accept().await, Err(Error::Protocol)));

    let (mut server, _peer) = connection(core::slice::from_ref(&request_frame)).await;
    let response = Response {
        headers: &[],
        body: &[],
        trailers: &[],
    };
    assert!(matches!(
        server.respond(1, response).await,
        Err(Error::Protocol)
    ));

    let (mut server, _peer) = connection(&[request_frame]).await;
    assert_eq!(server.accept().await.unwrap().stream_id, request.stream_id);
    let headers = [HeaderField {
        name: "x".into(),
        value: "a".repeat(4096),
    }];
    let response = Response {
        headers: &headers,
        body: &[],
        trailers: &[],
    };
    assert!(matches!(
        server.respond(request.stream_id, response).await,
        Err(Error::HeaderBlock)
    ));
}

#[tokio::test]
async fn reports_goaway_after_a_peer_resets_the_request() {
    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
        frame(FrameType::RstStream, 1, 0, &8_u32.to_be_bytes()),
        frame(FrameType::RstStream, 1, 0, &8_u32.to_be_bytes()),
        frame(FrameType::WindowUpdate, 1, 0, &encode_window_update(1)),
        frame(FrameType::GoAway, 0, 0, &[0; 8]),
    ])
    .await;
    assert!(matches!(
        server.accept().await,
        Err(Error::Reset {
            stream_id: 1,
            code: 8
        })
    ));
    assert!(matches!(server.accept().await, Err(Error::Closed)));
}
