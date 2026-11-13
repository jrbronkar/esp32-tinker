use embedded_http2::{BodyFrame, Error, HeaderField, Response, Server};
use embedded_io_adapters::tokio_1::FromTokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use zerodds_http2::{
    CLIENT_PREFACE, ErrorCode, Flags, FrameHeader, FrameType, Http2Error, encode_frame,
    flow::{INITIAL_WINDOW_SIZE, encode_window_update},
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

    for invalid in [
        frame(FrameType::Settings, 0, Flags::ACK, &[]),
        frame(FrameType::Ping, 0, 0, &[0; 8]),
    ] {
        let mut input = CLIENT_PREFACE.to_vec();
        input.extend(invalid);
        assert!(matches!(open(&input).await, Err(Error::Protocol)));
    }

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
async fn rejects_invalid_connection_and_stream_frames() -> Result<(), Box<dyn std::error::Error>> {
    let mut oversized = frame(FrameType::Data, 1, 0, &[]);
    let [_, length @ ..] = (DEFAULT_MAX_FRAME_SIZE + 1).to_be_bytes();
    oversized[..length.len()].copy_from_slice(&length);
    let (mut server, mut peer) = connection(&[oversized]).await;
    assert!(matches!(
        server.accept().await,
        Err(Error::Wire(Http2Error::FrameTooLarge { .. }))
    ));
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        goaway(CONNECTION, ErrorCode::FrameSizeError)
    );

    for invalid in [
        frame(FrameType::Settings, 0, Flags::ACK, &[0; 6]),
        frame(FrameType::Headers, 2, END_HEADERS | END_STREAM, REQUEST),
        frame(FrameType::WindowUpdate, 1, 0, &encode_window_update(1)),
        frame(FrameType::RstStream, 1, 0, &8_u32.to_be_bytes()),
    ] {
        let (mut server, _peer) = connection(&[invalid]).await;
        assert!(matches!(server.accept().await, Err(Error::Protocol)));
    }

    for invalid in [
        frame(FrameType::Settings, 1, 0, &[]),
        frame(FrameType::Ping, 1, 0, &[0; 8]),
        frame(FrameType::Ping, 0, 0, &[0; 7]),
        frame(FrameType::RstStream, 0, 0, &[0; 4]),
        frame(FrameType::RstStream, 1, 0, &[0; 3]),
        frame(FrameType::GoAway, 1, 0, &[0; 8]),
        frame(FrameType::GoAway, 0, 0, &[0; 7]),
        frame(FrameType::Priority, 0, 0, &[0; 5]),
        frame(FrameType::Priority, 1, 0, &[0; 4]),
    ] {
        let (mut server, _peer) = connection(&[invalid]).await;
        assert!(matches!(
            server.accept().await,
            Err(Error::Wire(Http2Error::Protocol(ErrorCode::ProtocolError)))
        ));
    }

    let (mut server, _peer) = connection(&[
        frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
        frame(FrameType::Data, 3, END_STREAM, b"wrong stream"),
    ])
    .await;
    assert!(matches!(server.accept().await, Err(Error::Protocol)));
    Ok(())
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
    for invalid in [
        frame(FrameType::Ping, 0, 0, &[0; 8]),
        frame(FrameType::Continuation, 3, END_HEADERS, REQUEST),
    ] {
        let (mut server, _peer) =
            connection(&[frame(FrameType::Headers, 1, 0, REQUEST), invalid]).await;
        assert!(matches!(server.accept().await, Err(Error::Protocol)));
    }

    for block in [
        vec![0x82; 4097],                             // Encoded block limit.
        vec![0x82; 150],                              // Decoded header list limit.
        zerodds_hpack::encode_integer(4097, 5, 0x20), // Dynamic table limit.
        vec![0x82, 0x20],                             // HPACK table updates must precede fields.
        vec![0xff],                                   // Truncated indexed field.
        vec![0x80],                                   // Index zero is invalid.
        vec![0, 1, b'x', 2, b'y'],                    // Truncated literal value.
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

#[tokio::test]
async fn reports_disconnect_during_frame_header_or_continuation() {
    let partial_header = frame(FrameType::Ping, 0, 0, &[0; 8]);
    for frames in [
        vec![partial_header[..4].to_vec()],
        vec![frame(FrameType::Headers, 1, END_STREAM, REQUEST)],
    ] {
        let (mut server, _peer) = connection(&frames).await;
        assert!(matches!(server.accept().await, Err(Error::Read(_))));
    }
}

#[tokio::test]
async fn reports_disconnect_while_sending_a_response() {
    for window in [0, 1024] {
        let (mut server, peer) = connection(&[
            frame(
                FrameType::Settings,
                0,
                0,
                &encode_settings(&[Setting {
                    id: SettingId::InitialWindowSize,
                    value: window,
                }]),
            ),
            frame(FrameType::Headers, 1, END_HEADERS | END_STREAM, REQUEST),
        ])
        .await;
        let request = server.accept().await.unwrap();
        // With a zero send window, keep the read half open so the server can
        // send headers before discovering EOF while waiting for credit.
        let peer = if window == 0 {
            Some(peer)
        } else {
            drop(peer);
            None
        };
        let response = Response {
            headers: &[HeaderField {
                name: ":status".into(),
                value: "200".into(),
            }],
            body: b"response",
            trailers: &[],
        };
        let result = server.respond(request.stream_id, response).await;
        assert!(if window == 0 {
            matches!(result, Err(Error::Read(_)))
        } else {
            matches!(result, Err(Error::Write(_)))
        });
        drop(peer);
    }
}

#[tokio::test]
async fn honors_the_peers_header_list_limit_for_trailers() {
    let (mut server, mut peer) = connection(&[
        frame(
            FrameType::Settings,
            0,
            0,
            &encode_settings(&[Setting {
                id: SettingId::MaxHeaderListSize,
                value: 64,
            }]),
        ),
        frame(FrameType::Headers, 1, END_HEADERS | END_STREAM, REQUEST),
    ])
    .await;
    let request = server.accept().await.unwrap();
    let response = Response {
        headers: &[HeaderField {
            name: ":status".into(),
            value: "200".into(),
        }],
        body: b"response",
        trailers: &[HeaderField {
            name: "x-result".into(),
            value: "x".repeat(64),
        }],
    };
    assert!(matches!(
        server.respond(request.stream_id, response).await,
        Err(Error::HeaderBlock)
    ));
    drop(server);
    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut output)
        .await
        .unwrap();
    let (settings, consumed) =
        zerodds_http2::decode_frame(&output, DEFAULT_MAX_FRAME_SIZE).unwrap();
    assert_eq!(settings.header.frame_type, FrameType::Settings);
    let ack = frame(FrameType::Settings, 0, Flags::ACK, &[]);
    assert_eq!(
        &output[consumed..],
        [ack.as_slice(), ack.as_slice()].concat()
    );
}

const FIRST_STREAM: u32 = 1;
const NEXT_STREAM: u32 = 3;
const CONNECTION: u32 = 0;
const NO_FLAGS: u8 = 0;

fn describe_transport_error(error: TestError) -> String {
    format!("{error:?}")
}

async fn read_wire_frame(peer: &mut DuplexStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0; FRAME_HEADER_LEN];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(&mut header),
    )
    .await??;
    // The payload length is the first 24 bits of the HTTP/2 frame header.
    let [len_hi, len_mid, len_lo, ..] = header;
    let len = u32::from_be_bytes([0, len_hi, len_mid, len_lo]) as usize;
    if len > DEFAULT_MAX_FRAME_SIZE as usize {
        return Err(std::io::Error::other("unexpected test frame length"));
    }
    let mut bytes = header.to_vec();
    bytes.resize(FRAME_HEADER_LEN + len, 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(&mut bytes[FRAME_HEADER_LEN..]),
    )
    .await??;
    Ok(bytes)
}

async fn expect_no_output(peer: &mut DuplexStream) -> std::io::Result<()> {
    let mut byte = [0];
    tokio::select! {
        biased;
        result = peer.read(&mut byte) => Err(std::io::Error::other(format!("unexpected read: {result:?}"))),
        _ = std::future::ready(()) => Ok(()),
    }
}

fn window_update(stream_id: u32, credit: usize) -> Vec<u8> {
    frame(
        FrameType::WindowUpdate,
        stream_id,
        NO_FLAGS,
        &encode_window_update(credit as u32),
    )
}

#[tokio::test]
async fn incremental_body_returns_credit_on_release_and_response()
-> Result<(), Box<dyn std::error::Error>> {
    const PAD_LENGTH: u8 = 1;
    const PADDING: u8 = 0;
    const PAYLOAD: u8 = b'a';
    let padded_data = [PAD_LENGTH, PAYLOAD, PADDING];
    let final_data = b"end";
    let (mut server, mut peer) = connection(&[
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
        frame(FrameType::Data, FIRST_STREAM, Flags::PADDED, &padded_data),
        frame(FrameType::Data, FIRST_STREAM, END_STREAM, final_data),
    ])
    .await;
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;

    let head = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(head.stream_id, FIRST_STREAM);
    assert!(!head.end_stream);
    assert!(head.headers.iter().any(|field| field.name == ":method"));
    assert!(matches!(
        server.read_body(head.stream_id).await.map_err(describe_transport_error)?,
        Some(BodyFrame::Data(bytes)) if bytes == [PAYLOAD]
    ));
    expect_no_output(&mut peer).await?;
    server
        .release_capacity(head.stream_id)
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        window_update(FIRST_STREAM, padded_data.len())
    );
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        window_update(CONNECTION, padded_data.len())
    );
    server
        .release_capacity(head.stream_id)
        .await
        .map_err(describe_transport_error)?;
    expect_no_output(&mut peer).await?;

    assert!(matches!(
        server.read_body(head.stream_id).await.map_err(describe_transport_error)?,
        Some(BodyFrame::Data(bytes)) if bytes == final_data
    ));
    assert!(
        server
            .read_body(head.stream_id)
            .await
            .map_err(describe_transport_error)?
            .is_none()
    );
    expect_no_output(&mut peer).await?;

    server
        .respond(
            head.stream_id,
            Response {
                headers: &[HeaderField {
                    name: ":status".into(),
                    value: "204".into(),
                }],
                body: &[],
                trailers: &[],
            },
        )
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        window_update(CONNECTION, final_data.len())
    );
    Ok(())
}

#[tokio::test]
async fn incremental_reset_reclaims_connection_credit_for_next_request()
-> Result<(), Box<dyn std::error::Error>> {
    let pending = b"abandoned";
    let cancel = (ErrorCode::Cancel as u32).to_be_bytes();
    let (mut server, mut peer) = connection(&[
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
        frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, pending),
        frame(FrameType::RstStream, FIRST_STREAM, NO_FLAGS, &cancel),
        frame(
            FrameType::Headers,
            NEXT_STREAM,
            END_HEADERS | END_STREAM,
            REQUEST,
        ),
    ])
    .await;
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    let head = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    assert!(matches!(
        server.read_body(head.stream_id).await.map_err(describe_transport_error)?,
        Some(BodyFrame::Data(bytes)) if bytes == pending
    ));
    assert!(matches!(
        server.read_body(head.stream_id).await,
        Err(Error::Reset { stream_id: FIRST_STREAM, code }) if code == ErrorCode::Cancel as u32
    ));
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        window_update(CONNECTION, pending.len())
    );
    expect_no_output(&mut peer).await?;
    let next = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(next.stream_id, NEXT_STREAM);
    assert!(next.end_stream);
    Ok(())
}

#[tokio::test]
async fn incremental_body_rejects_invalid_stream_transitions()
-> Result<(), Box<dyn std::error::Error>> {
    let invalid_frames = [
        frame(FrameType::Data, NEXT_STREAM, END_STREAM, b"wrong stream"),
        frame(
            FrameType::Headers,
            NEXT_STREAM,
            END_HEADERS | END_STREAM,
            &[],
        ),
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, &[]),
        // Request pseudo-headers are forbidden in trailers.
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            REQUEST,
        ),
    ];
    for (index, invalid) in invalid_frames.into_iter().enumerate() {
        let (mut server, _peer) = connection(&[
            frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
            invalid,
        ])
        .await;
        let head = server
            .accept_head()
            .await
            .map_err(describe_transport_error)?;
        assert!(matches!(
            server.read_body(NEXT_STREAM).await,
            Err(Error::Protocol)
        ));
        assert!(matches!(
            server.release_capacity(NEXT_STREAM).await,
            Err(Error::Protocol)
        ));
        let result = server.read_body(head.stream_id).await;
        if index < 2 {
            assert!(matches!(result, Err(Error::Protocol)));
        } else {
            assert!(matches!(result, Err(Error::MalformedRequest)));
        }
    }
    Ok(())
}

#[tokio::test]
async fn incremental_body_enforces_unreleased_receive_window()
-> Result<(), Box<dyn std::error::Error>> {
    let window = INITIAL_WINDOW_SIZE as usize;
    let payload = vec![0; window];
    let mut frames = vec![frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_HEADERS,
        REQUEST,
    )];
    frames.extend(
        payload
            .chunks(DEFAULT_MAX_FRAME_SIZE as usize)
            .map(|chunk| frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, chunk)),
    );
    frames.push(frame(FrameType::Data, FIRST_STREAM, END_STREAM, b"x"));
    let (mut server, _peer) = connection(&frames).await;
    let head = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    let mut received = 0;
    while received < window {
        match server
            .read_body(head.stream_id)
            .await
            .map_err(describe_transport_error)?
        {
            Some(BodyFrame::Data(bytes)) if !bytes.is_empty() => received += bytes.len(),
            other => return Err(format!("expected nonempty DATA, got {other:?}").into()),
        }
    }
    assert_eq!(received, window);
    assert!(matches!(
        server.read_body(head.stream_id).await,
        Err(Error::Wire(Http2Error::FlowControlExceeded))
    ));
    Ok(())
}

#[tokio::test]
async fn incremental_headers_do_not_wait_for_data_and_reject_invalid_trailers()
-> Result<(), Box<dyn std::error::Error>> {
    let initial = frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST);
    let (mut server, _peer) = connection(core::slice::from_ref(&initial)).await;
    let head = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    assert!(!head.end_stream);
    assert!(matches!(
        server.read_body(head.stream_id).await,
        Err(Error::Read(_))
    ));

    // RFC 7541 reserves index zero. The indexed-field prefix alone is invalid.
    const INVALID_INDEXED_FIELD: &[u8] = &[0b1000_0000];
    let (mut server, _peer) = connection(&[
        initial,
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            INVALID_INDEXED_FIELD,
        ),
    ])
    .await;
    let head = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?;
    assert!(matches!(
        server.read_body(head.stream_id).await,
        Err(Error::HeaderBlock)
    ));
    Ok(())
}

#[tokio::test]
async fn credit_release_propagates_stream_and_connection_write_failures() {
    for flags in [NO_FLAGS, END_STREAM] {
        let (mut server, peer) = connection(&[
            frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
            frame(FrameType::Data, FIRST_STREAM, flags, b"data"),
        ])
        .await;
        drop(peer);
        assert!(matches!(server.accept().await, Err(Error::Write(_))));
    }
}

#[tokio::test]
async fn incremental_response_enforces_order_and_can_finish_without_credit()
-> Result<(), Box<dyn std::error::Error>> {
    let settings = encode_settings(&[Setting {
        id: SettingId::InitialWindowSize,
        value: 0,
    }]);
    let (mut server, mut peer) = connection(&[
        frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings),
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
        frame(FrameType::Data, FIRST_STREAM, END_STREAM, &[]),
    ])
    .await;
    let _settings = read_wire_frame(&mut peer).await?;
    let _initial_ack = read_wire_frame(&mut peer).await?;
    let id = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?
        .stream_id;
    let _settings_ack = read_wire_frame(&mut peer).await?;
    let head = [HeaderField {
        name: ":status".into(),
        value: "200".into(),
    }];
    assert!(matches!(
        server.send_data(id, b"data").await,
        Err(Error::Protocol)
    ));
    assert!(matches!(
        server.wait_send_capacity(id).await,
        Err(Error::Protocol)
    ));
    assert!(matches!(
        server.start_response(NEXT_STREAM, &head).await,
        Err(Error::Protocol)
    ));
    server
        .start_response(id, &head)
        .await
        .map_err(describe_transport_error)?;
    let _response_head = read_wire_frame(&mut peer).await?;
    assert!(matches!(
        server.start_response(id, &head).await,
        Err(Error::Protocol)
    ));
    assert!(matches!(
        server.finish_response(id, &[]).await,
        Err(Error::Protocol)
    ));
    assert!(matches!(
        server.wait_send_capacity(id).await.map_err(describe_transport_error)?,
        embedded_http2::SendEvent::RequestBody(BodyFrame::Data(bytes)) if bytes.is_empty()
    ));
    assert_eq!(
        server
            .send_data(id, b"blocked")
            .await
            .map_err(describe_transport_error)?,
        0
    );
    expect_no_output(&mut peer).await?;
    server
        .finish_response(id, &[])
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        frame(FrameType::Data, id, END_STREAM, &[])
    );
    Ok(())
}

#[tokio::test]
async fn incremental_response_obeys_the_connection_window_and_frame_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let connection_window = INITIAL_WINDOW_SIZE as usize;
    let body = vec![b'x'; connection_window + 1];
    let settings = encode_settings(&[Setting {
        id: SettingId::InitialWindowSize,
        value: body.len() as u32,
    }]);
    let (mut server, _peer) = connection(&[
        frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            REQUEST,
        ),
        window_update(CONNECTION, 1),
    ])
    .await;
    let id = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?
        .stream_id;
    let head = [HeaderField {
        name: ":status".into(),
        value: "200".into(),
    }];
    server
        .start_response(id, &head)
        .await
        .map_err(describe_transport_error)?;
    let mut sent = 0;
    while sent < connection_window {
        let remaining = body.get(sent..).ok_or("invalid response cursor")?;
        let written = server
            .send_data(id, remaining)
            .await
            .map_err(describe_transport_error)?;
        assert_eq!(
            written,
            (connection_window - sent).min(DEFAULT_MAX_FRAME_SIZE as usize)
        );
        sent += written;
    }
    let tail = body.get(sent..).ok_or("missing response tail")?;
    assert_eq!(
        server
            .send_data(id, tail)
            .await
            .map_err(describe_transport_error)?,
        0
    );
    assert!(matches!(
        server
            .wait_send_capacity(id)
            .await
            .map_err(describe_transport_error)?,
        embedded_http2::SendEvent::Ready
    ));
    assert_eq!(
        server
            .send_data(id, tail)
            .await
            .map_err(describe_transport_error)?,
        tail.len()
    );
    server
        .finish_response(id, &[])
        .await
        .map_err(describe_transport_error)?;
    Ok(())
}

#[tokio::test]
async fn incremental_wait_surfaces_request_trailers_and_peer_reset()
-> Result<(), Box<dyn std::error::Error>> {
    let settings = encode_settings(&[Setting {
        id: SettingId::InitialWindowSize,
        value: 0,
    }]);
    let cancel = (ErrorCode::Cancel as u32).to_be_bytes();
    let (mut server, _peer) = connection(&[
        frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings),
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &[],
        ),
        frame(FrameType::RstStream, FIRST_STREAM, NO_FLAGS, &cancel),
    ])
    .await;
    let id = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?
        .stream_id;
    let head = [HeaderField {
        name: ":status".into(),
        value: "200".into(),
    }];
    server
        .start_response(id, &head)
        .await
        .map_err(describe_transport_error)?;
    assert!(matches!(
        server.wait_send_capacity(id).await.map_err(describe_transport_error)?,
        embedded_http2::SendEvent::RequestBody(BodyFrame::Trailers(fields)) if fields.is_empty()
    ));
    assert!(matches!(
        server.wait_send_capacity(id).await,
        Err(Error::Reset { stream_id: FIRST_STREAM, code }) if code == ErrorCode::Cancel as u32
    ));
    Ok(())
}

const REQUEST_FIELDS: &[(&str, &str)] = &[(":method", "POST"), (":scheme", "http"), (":path", "/")];

fn header_block(fields: &[(&str, &str)]) -> Vec<u8> {
    let fields: Vec<_> = fields
        .iter()
        .map(|&(name, value)| HeaderField {
            name: name.into(),
            value: value.into(),
        })
        .collect();
    zerodds_hpack::Encoder::new().encode(&fields)
}

#[tokio::test]
async fn rejects_malformed_request_fields_before_dispatch() {
    for extra in [
        (":status", "200"),
        (":method", "POST"),
        (":unknown", "value"),
        ("Uppercase", "value"),
        ("bad name", "value"),
        ("x-name", " leading"),
        ("x-name", "line\nbreak"),
        ("connection", "close"),
        ("te", "gzip"),
        ("content-length", "+1"),
        ("content-length", "18446744073709551616"),
    ] {
        let mut fields = REQUEST_FIELDS.to_vec();
        fields.push(extra);
        let (mut server, _peer) = connection(&[frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &header_block(&fields),
        )])
        .await;
        assert!(
            matches!(server.accept_head().await, Err(Error::MalformedRequest)),
            "{extra:?}"
        );
    }

    for fields in [
        &REQUEST_FIELDS[..2],
        &[(":method", "GET"), (":scheme", "http"), (":path", "")],
        &[
            (":method", "GET"),
            ("x-name", "value"),
            (":scheme", "http"),
            (":path", "/"),
        ],
        &[(":method", "CONNECT")],
    ] {
        let (mut server, _peer) = connection(&[frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &header_block(fields),
        )])
        .await;
        assert!(
            matches!(server.accept_head().await, Err(Error::MalformedRequest)),
            "{fields:?}"
        );
    }
}

#[tokio::test]
async fn accepts_ordinary_requests_and_connect() -> Result<(), Box<dyn std::error::Error>> {
    for fields in [
        &[(":method", "CONNECT"), (":authority", "localhost:443")][..],
        &[
            (":method", "POST"),
            (":scheme", "http"),
            (":path", "/"),
            ("te", "Trailers"),
            ("content-length", "0"),
            ("content-length", "0"),
        ],
    ] {
        let (mut server, _peer) = connection(&[frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &header_block(fields),
        )])
        .await;
        assert!(
            server
                .accept_head()
                .await
                .map_err(describe_transport_error)?
                .end_stream
        );
    }
    Ok(())
}

#[tokio::test]
async fn request_length_excludes_padding_and_checks_all_half_close_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fields = REQUEST_FIELDS.to_vec();
    fields.push(("content-length", "2"));
    let block = header_block(&fields);
    let initial = frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, &block);
    let (mut server, mut peer) = connection(&[frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_HEADERS | END_STREAM,
        &block,
    )])
    .await;
    assert!(matches!(
        server.accept_head().await,
        Err(Error::MalformedRequest)
    ));
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        frame(
            FrameType::RstStream,
            FIRST_STREAM,
            NO_FLAGS,
            &(ErrorCode::ProtocolError as u32).to_be_bytes(),
        )
    );

    for body_end in [
        frame(FrameType::Data, FIRST_STREAM, END_STREAM, b"x"),
        frame(FrameType::Data, FIRST_STREAM, END_STREAM, b"too long"),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &[],
        ),
    ] {
        let (mut server, _peer) = connection(&[initial.clone(), body_end]).await;
        assert!(matches!(
            server.accept().await,
            Err(Error::MalformedRequest)
        ));
    }

    const PADDED_BYTE: &[u8] = &[1, b'a', 0]; // Pad Length, DATA, one padding octet.
    let (mut server, _peer) = connection(&[
        initial,
        frame(FrameType::Data, FIRST_STREAM, Flags::PADDED, PADDED_BYTE),
        frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"b"),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &[],
        ),
    ])
    .await;
    assert_eq!(
        server
            .accept()
            .await
            .map_err(describe_transport_error)?
            .body,
        b"ab"
    );

    fields.push(("content-length", "3"));
    let (mut server, _peer) = connection(&[frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_HEADERS,
        &header_block(&fields),
    )])
    .await;
    assert!(matches!(
        server.accept_head().await,
        Err(Error::MalformedRequest)
    ));
    Ok(())
}

#[tokio::test]
async fn rejects_response_pseudo_trailers_before_writing_them()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut server, mut peer) = connection(&[frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_HEADERS | END_STREAM,
        REQUEST,
    )])
    .await;
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    let id = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?
        .stream_id;
    let head = [HeaderField {
        name: ":status".into(),
        value: http::StatusCode::OK.as_str().into(),
    }];
    server
        .start_response(id, &head)
        .await
        .map_err(describe_transport_error)?;
    let _head = read_wire_frame(&mut peer).await?;
    assert!(matches!(
        server.finish_response(id, &head).await,
        Err(Error::Protocol)
    ));
    expect_no_output(&mut peer).await?;
    server
        .finish_response(id, &[])
        .await
        .map_err(describe_transport_error)?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        frame(FrameType::Data, id, END_STREAM, &[])
    );
    Ok(())
}

#[tokio::test]
async fn invalid_padding_takes_precedence_over_receive_window_exhaustion()
-> Result<(), Box<dyn std::error::Error>> {
    let window = INITIAL_WINDOW_SIZE as usize;
    let payload = vec![0; window];
    let mut frames = vec![frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_HEADERS,
        REQUEST,
    )];
    frames.extend(
        payload
            .chunks(DEFAULT_MAX_FRAME_SIZE as usize)
            .map(|chunk| frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, chunk)),
    );
    const PAD_LENGTH_WITHOUT_PADDING: &[u8] = &[1];
    frames.push(frame(
        FrameType::Data,
        FIRST_STREAM,
        Flags::PADDED,
        PAD_LENGTH_WITHOUT_PADDING,
    ));
    let (mut server, _peer) = connection(&frames).await;
    let id = server
        .accept_head()
        .await
        .map_err(describe_transport_error)?
        .stream_id;
    let mut remaining = window;
    while remaining > 0 {
        match server
            .read_body(id)
            .await
            .map_err(describe_transport_error)?
        {
            Some(BodyFrame::Data(bytes)) if !bytes.is_empty() => {
                remaining = remaining
                    .checked_sub(bytes.len())
                    .ok_or("received more than the window")?;
            }
            other => return Err(format!("expected nonempty DATA, got {other:?}").into()),
        }
    }
    assert!(matches!(server.read_body(id).await, Err(Error::Protocol)));
    Ok(())
}

fn goaway(last_stream: u32, code: ErrorCode) -> Vec<u8> {
    let mut payload = last_stream.to_be_bytes().to_vec();
    payload.extend_from_slice(&(code as u32).to_be_bytes());
    frame(FrameType::GoAway, CONNECTION, NO_FLAGS, &payload)
}

#[tokio::test]
async fn window_overflow_sends_an_error_in_the_right_scope()
-> Result<(), Box<dyn std::error::Error>> {
    // RFC 9113 §6.9.1 caps a window at 2^31−1. Both windows start nonzero.
    const MAX_WINDOW_INCREMENT: usize = i32::MAX as usize;
    let code = ErrorCode::FlowControlError;
    for (target, expected) in [
        (CONNECTION, goaway(FIRST_STREAM, code)),
        (
            FIRST_STREAM,
            frame(
                FrameType::RstStream,
                FIRST_STREAM,
                NO_FLAGS,
                &(code as u32).to_be_bytes(),
            ),
        ),
    ] {
        let (mut server, mut peer) = connection(&[
            frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
            window_update(target, MAX_WINDOW_INCREMENT),
        ])
        .await;
        let result = server.accept().await;
        if target == CONNECTION {
            assert!(matches!(
                result,
                Err(Error::Wire(Http2Error::Protocol(
                    ErrorCode::FlowControlError
                )))
            ));
        } else {
            assert!(
                matches!(result, Err(Error::Reset { stream_id: FIRST_STREAM, code: wire_code }) if wire_code == code as u32)
            );
        }
        let _settings = read_wire_frame(&mut peer).await?;
        let _ack = read_wire_frame(&mut peer).await?;
        assert_eq!(read_wire_frame(&mut peer).await?, expected);
    }
    Ok(())
}

#[tokio::test]
async fn stray_continuation_sends_connection_protocol_error()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut server, mut peer) = connection(&[frame(
        FrameType::Continuation,
        FIRST_STREAM,
        END_HEADERS,
        &[],
    )])
    .await;
    assert!(matches!(
        server.accept().await,
        Err(Error::Wire(Http2Error::Protocol(ErrorCode::ProtocolError)))
    ));
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        goaway(CONNECTION, ErrorCode::ProtocolError)
    );
    Ok(())
}

#[tokio::test]
async fn multiplexing_refuses_excess_streams_and_isolates_flow_control_errors()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::Event;

    let (socket, mut peer) = duplex(4096);
    peer.write_all(CLIENT_PREFACE).await?;
    for packet in [
        frame(FrameType::Settings, 0, 0, &[]),
        frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
        frame(FrameType::Headers, 3, END_HEADERS, REQUEST),
        frame(FrameType::Headers, 5, END_HEADERS | END_STREAM, REQUEST),
        frame(FrameType::Data, 1, 0, b"pending"),
        window_update(1, i32::MAX as usize),
        frame(FrameType::Data, 3, END_STREAM, b"healthy"),
        frame(FrameType::Headers, 7, END_HEADERS | END_STREAM, REQUEST),
    ] {
        peer.write_all(&packet).await?;
    }
    let mut server = Server::handshake(FromTokio::new(socket))
        .await
        .map_err(|error| format!("{error:?}"))?;
    for id in [1, 3] {
        assert!(
            matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == id)
        );
    }
    assert!(matches!(
        server.next_event().await,
        Ok(Event::Reset { stream_id: 5, code }) if code == ErrorCode::RefusedStream as u32
    ));
    assert!(matches!(
        server.next_event().await,
        Ok(Event::Body {
            stream_id: 1,
            frame: BodyFrame::Data(b"pending"),
            end_stream: false
        })
    ));
    assert!(matches!(
        server.next_event().await,
        Ok(Event::Reset { stream_id: 1, code }) if code == ErrorCode::FlowControlError as u32
    ));
    assert!(matches!(
        server.next_event().await,
        Ok(Event::Body {
            stream_id: 3,
            frame: BodyFrame::Data(b"healthy"),
            end_stream: true
        })
    ));
    assert!(matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == 7));

    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    for (id, code) in [
        (5, ErrorCode::RefusedStream),
        (1, ErrorCode::FlowControlError),
    ] {
        assert_eq!(
            read_wire_frame(&mut peer).await?,
            frame(
                FrameType::RstStream,
                id,
                NO_FLAGS,
                &(code as u32).to_be_bytes()
            )
        );
    }
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        window_update(CONNECTION, b"pending".len())
    );
    Ok(())
}

#[tokio::test]
async fn configured_stream_limit_is_advertised_and_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    use core::num::NonZeroU32;
    use embedded_http2::{Config, Event};

    for limit in [NonZeroU32::MIN, NonZeroU32::MIN.saturating_add(2)] {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&frame(FrameType::Settings, 0, 0, &[]))
            .await?;
        for id in (1..=2 * limit.get() + 1).step_by(2) {
            peer.write_all(&frame(FrameType::Headers, id, END_HEADERS, REQUEST))
                .await?;
        }

        let mut server = Server::handshake_with_config(
            FromTokio::new(socket),
            Config {
                max_concurrent_streams: limit,
            },
        )
        .await
        .map_err(describe_transport_error)?;
        let settings = read_wire_frame(&mut peer).await?;
        assert!(settings[FRAME_HEADER_LEN..].chunks_exact(6).any(|setting| {
            setting[..2] == (SettingId::MaxConcurrentStreams as u16).to_be_bytes()
                && setting[2..] == limit.get().to_be_bytes()
        }));
        let _ack = read_wire_frame(&mut peer).await?;

        for id in (1..2 * limit.get()).step_by(2) {
            assert!(
                matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == id)
            );
        }
        assert!(matches!(server.next_event().await,
            Ok(Event::Reset { stream_id, code })
                if stream_id == 2 * limit.get() + 1 && code == ErrorCode::RefusedStream as u32));
    }
    Ok(())
}

struct EchoBuffered;

impl embedded_http2::service::Service for EchoBuffered {
    async fn call(
        &mut self,
        request: embedded_http2::service::Request,
    ) -> embedded_http2::service::Response {
        embedded_http2::service::Response {
            headers: vec![HeaderField {
                name: ":status".into(),
                value: "200".into(),
            }],
            body: request.body,
            trailers: request.trailers,
        }
    }
}

async fn serve_buffered_frames(
    exchanges: &[(&[Vec<u8>], &[u32])],
) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let (socket, mut peer) = duplex(128 * 1024);
    let client = async {
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&frame(FrameType::Settings, 0, 0, &[]))
            .await?;
        let mut responses = Vec::new();
        for (frames, completed_streams) in exchanges {
            for packet in *frames {
                peer.write_all(packet).await?;
            }
            let mut pending = completed_streams.to_vec();
            while !pending.is_empty() {
                let packet = read_wire_frame(&mut peer).await?;
                if matches!(
                    FrameType::from_u8(packet[3]),
                    Ok(FrameType::Data | FrameType::Headers)
                ) && packet[4] & END_STREAM != 0
                {
                    pending.retain(|stream| packet[5..9] != stream.to_be_bytes());
                }
                responses.push(packet);
            }
        }
        peer.shutdown().await?;
        Ok::<_, std::io::Error>(responses)
    };
    let (server, responses) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut service = EchoBuffered;
        tokio::join!(
            embedded_http2::service::serve(FromTokio::new(socket), &mut service),
            client
        )
    })
    .await?;
    assert!(matches!(
        server,
        Err(Error::Read(
            embedded_io_async::ReadExactError::UnexpectedEof
        ))
    ));
    Ok(responses?)
}

#[tokio::test]
async fn buffered_service_advances_other_streams_and_reuses_canceled_slot()
-> Result<(), Box<dyn std::error::Error>> {
    let settings = encode_settings(&[Setting {
        id: SettingId::InitialWindowSize,
        value: 1,
    }]);
    let responses = serve_buffered_frames(&[
        (
            &[
                frame(FrameType::Settings, 0, 0, &settings),
                frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
                frame(FrameType::Data, 1, END_STREAM, b"ab"),
                frame(FrameType::Headers, 3, END_HEADERS, REQUEST),
                frame(FrameType::Data, 3, END_STREAM, b"z"),
            ],
            &[3],
        ),
        (
            &[
                frame(FrameType::Headers, 5, END_HEADERS, REQUEST),
                frame(
                    FrameType::RstStream,
                    1,
                    NO_FLAGS,
                    &(ErrorCode::Cancel as u32).to_be_bytes(),
                ),
                frame(FrameType::Headers, 7, END_HEADERS, REQUEST),
                frame(FrameType::Data, 7, END_STREAM, b"q"),
                frame(FrameType::Data, 5, END_STREAM, b"r"),
            ],
            &[5, 7],
        ),
    ])
    .await?;
    for (id, bytes) in [(1_u32, b"a"), (3, b"z"), (5, b"r"), (7, b"q")] {
        let data: Vec<_> = responses
            .iter()
            .filter(|packet| packet[3] == FrameType::Data as u8 && packet[5..9] == id.to_be_bytes())
            .cloned()
            .collect();
        let mut expected = vec![frame(FrameType::Data, id, NO_FLAGS, bytes)];
        if id != 1 {
            expected.push(frame(FrameType::Data, id, END_STREAM, &[]));
        }
        assert_eq!(data, expected);
    }
    Ok(())
}

#[tokio::test]
async fn buffered_service_resets_an_oversized_request_and_completes_its_peer()
-> Result<(), Box<dyn std::error::Error>> {
    let full_frame = vec![0; DEFAULT_MAX_FRAME_SIZE as usize];
    let responses = serve_buffered_frames(&[(
        &[
            frame(FrameType::Headers, 1, END_HEADERS, REQUEST),
            frame(FrameType::Headers, 3, END_HEADERS, REQUEST),
            frame(FrameType::Data, 1, NO_FLAGS, &full_frame),
            frame(FrameType::Data, 1, END_STREAM, b"extra"),
            frame(FrameType::Data, 3, NO_FLAGS, b"ok"),
            // HPACK literal without indexing, new name "x", value "y".
            frame(
                FrameType::Headers,
                3,
                END_HEADERS | END_STREAM,
                &[0, 1, b'x', 1, b'y'],
            ),
        ],
        &[3],
    )])
    .await?;
    let reset = frame(
        FrameType::RstStream,
        1,
        NO_FLAGS,
        &(ErrorCode::Cancel as u32).to_be_bytes(),
    );
    let data = frame(FrameType::Data, 3, NO_FLAGS, b"ok");
    assert!(responses.contains(&reset));
    assert!(responses.contains(&data));
    assert!(
        responses
            .iter()
            .any(|packet| packet[3] == FrameType::Headers as u8
                && packet[4] == END_HEADERS | END_STREAM
                && packet[5..9] == 3_u32.to_be_bytes())
    );
    Ok(())
}

#[tokio::test]
async fn multiplexing_resets_a_half_closed_stream_without_interrupting_its_peer()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::Event;

    for late_frame in [
        frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"late"),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            &[],
        ),
    ] {
        let (mut server, mut peer) = connection(&[
            frame(
                FrameType::Headers,
                FIRST_STREAM,
                END_HEADERS | END_STREAM,
                REQUEST,
            ),
            frame(FrameType::Headers, NEXT_STREAM, END_HEADERS, REQUEST),
            late_frame,
            frame(FrameType::Data, NEXT_STREAM, END_STREAM, b"ok"),
        ])
        .await;
        for id in [FIRST_STREAM, NEXT_STREAM] {
            assert!(
                matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == id)
            );
        }
        assert!(
            matches!(server.next_event().await, Ok(Event::Reset { stream_id: FIRST_STREAM, code }) if code == ErrorCode::StreamClosed as u32)
        );
        assert!(matches!(
            server.next_event().await,
            Ok(Event::Body {
                stream_id: NEXT_STREAM,
                frame: BodyFrame::Data(b"ok"),
                end_stream: true
            })
        ));
        let _settings = read_wire_frame(&mut peer).await?;
        let _ack = read_wire_frame(&mut peer).await?;
        assert_eq!(
            read_wire_frame(&mut peer).await?,
            frame(
                FrameType::RstStream,
                FIRST_STREAM,
                NO_FLAGS,
                &(ErrorCode::StreamClosed as u32).to_be_bytes()
            )
        );
    }
    Ok(())
}

#[tokio::test]
async fn multiplexing_sends_goaway_for_headers_on_an_unavailable_stream()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::Event;

    for (id, code) in [
        (NEXT_STREAM, ErrorCode::StreamClosed),
        (FIRST_STREAM, ErrorCode::ProtocolError),
    ] {
        let (mut server, mut peer) = connection(&[
            frame(
                FrameType::Headers,
                NEXT_STREAM,
                END_HEADERS | END_STREAM,
                REQUEST,
            ),
            frame(
                FrameType::RstStream,
                NEXT_STREAM,
                NO_FLAGS,
                &(ErrorCode::Cancel as u32).to_be_bytes(),
            ),
            frame(FrameType::Headers, id, END_HEADERS | END_STREAM, REQUEST),
        ])
        .await;
        assert!(
            matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == NEXT_STREAM)
        );
        assert!(matches!(
            server.next_event().await,
            Ok(Event::Reset {
                stream_id: NEXT_STREAM,
                ..
            })
        ));
        assert!(matches!(server.next_event().await, Err(Error::Protocol)));
        let _settings = read_wire_frame(&mut peer).await?;
        let _ack = read_wire_frame(&mut peer).await?;
        assert_eq!(read_wire_frame(&mut peer).await?, goaway(NEXT_STREAM, code));
    }
    Ok(())
}

#[tokio::test]
async fn multiplexing_reports_malformed_trailers_as_a_stream_reset()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::Event;

    let (mut server, mut peer) = connection(&[
        frame(FrameType::Headers, FIRST_STREAM, END_HEADERS, REQUEST),
        frame(FrameType::Headers, NEXT_STREAM, END_HEADERS, REQUEST),
        frame(
            FrameType::Headers,
            FIRST_STREAM,
            END_HEADERS | END_STREAM,
            REQUEST,
        ),
        frame(FrameType::Data, NEXT_STREAM, END_STREAM, b"ok"),
    ])
    .await;
    for id in [FIRST_STREAM, NEXT_STREAM] {
        assert!(
            matches!(server.next_event().await, Ok(Event::Request(head)) if head.stream_id == id)
        );
    }
    assert!(
        matches!(server.next_event().await, Ok(Event::Reset { stream_id: FIRST_STREAM, code }) if code == ErrorCode::ProtocolError as u32)
    );
    assert!(matches!(
        server.next_event().await,
        Ok(Event::Body {
            stream_id: NEXT_STREAM,
            frame: BodyFrame::Data(b"ok"),
            end_stream: true
        })
    ));
    let _settings = read_wire_frame(&mut peer).await?;
    let _ack = read_wire_frame(&mut peer).await?;
    assert_eq!(
        read_wire_frame(&mut peer).await?,
        frame(
            FrameType::RstStream,
            FIRST_STREAM,
            NO_FLAGS,
            &(ErrorCode::ProtocolError as u32).to_be_bytes()
        )
    );
    Ok(())
}

#[tokio::test]
async fn multiplexed_read_resumes_in_a_continuation_block() -> Result<(), Box<dyn std::error::Error>>
{
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        task::Poll,
    };
    use embedded_http2::Event;

    let (socket, mut peer) = duplex(4096);
    peer.write_all(CLIENT_PREFACE).await?;
    peer.write_all(&frame(FrameType::Settings, 0, 0, &[]))
        .await?;
    let mut server = Server::handshake(FromTokio::new(socket))
        .await
        .map_err(describe_transport_error)?;
    peer.write_all(&frame(
        FrameType::Headers,
        FIRST_STREAM,
        END_STREAM,
        &REQUEST[..1],
    ))
    .await?;
    peer.write_all(&frame(
        FrameType::Continuation,
        FIRST_STREAM,
        NO_FLAGS,
        &REQUEST[1..2],
    ))
    .await?;
    let last = frame(
        FrameType::Continuation,
        FIRST_STREAM,
        END_HEADERS,
        &REQUEST[2..],
    );
    peer.write_all(&last[..4]).await?;

    {
        let mut read = pin!(server.next_event());
        poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    peer.write_all(&last[4..]).await?;
    let event = server
        .next_event()
        .await
        .map_err(describe_transport_error)?;
    assert!(
        matches!(event, Event::Request(head) if head.stream_id == FIRST_STREAM && head.end_stream && head.headers.len() == 3)
    );
    Ok(())
}

#[tokio::test]
async fn receive_handles_release_credit_and_report_reset() -> Result<(), Box<dyn std::error::Error>>
{
    use embedded_http2::server::{self, StreamError};
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&frame(FrameType::Settings, 0, 0, &[])).await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;

        let (next_released, next_credit_seen) = tokio::sync::oneshot::channel();
        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            for id in [FIRST_STREAM, NEXT_STREAM] {
                peer.write_all(&frame(FrameType::Headers, id, END_HEADERS, REQUEST)).await?;
            }
            peer.write_all(&frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"first")).await?;
            peer.write_all(&frame(FrameType::Data, NEXT_STREAM, END_STREAM, b"next")).await?;
            let update = read_wire_frame(&mut peer).await?;
            assert_eq!(update, window_update(CONNECTION, 4));
            next_released.send(()).map_err(|_| "handler exited")?;
            assert_eq!(read_wire_frame(&mut peer).await?, window_update(FIRST_STREAM, 5));
            assert_eq!(read_wire_frame(&mut peer).await?, window_update(CONNECTION, 5));
            peer.write_all(&frame(FrameType::RstStream, FIRST_STREAM, NO_FLAGS, &(ErrorCode::Cancel as u32).to_be_bytes())).await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handler = async {
            let mut first_request = incoming.accept().await.ok_or("missing first request")?;
            let first = &mut first_request.body;
            let mut next_request = incoming.accept().await.ok_or("missing next request")?;
            let next = &mut next_request.body;
            let chunk = first.data().await.map_err(|error| format!("{error:?}"))?.ok_or("missing first chunk")?;
            {
                let next_chunk = next.data().await.map_err(|error| format!("{error:?}"))?.ok_or("missing next chunk")?;
                assert_eq!(next_chunk.as_bytes(), b"next");
            }
            next_credit_seen.await?;
            assert_eq!(chunk.as_bytes(), b"first");
            drop(chunk);
            assert!(matches!(first.data().await, Err(StreamError::Reset(code)) if code == ErrorCode::Cancel as u32));
            assert!(matches!(next.data().await, Ok(None)));
            drop(incoming);
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handler) = tokio::join!(connection.run(), client, handler);
        client?;
        handler?;
        driver.map_err(describe_transport_error)?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }).await?
}

#[tokio::test]
async fn receive_handles_deliver_trailers_cancel_dropped_requests_and_report_disconnect()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::server::{self, StreamError};
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&frame(FrameType::Settings, 0, NO_FLAGS, &[]))
            .await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;
        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            for id in [FIRST_STREAM, NEXT_STREAM] {
                peer.write_all(&frame(FrameType::Headers, id, END_HEADERS, REQUEST))
                    .await?;
            }
            peer.write_all(&frame(FrameType::Data, NEXT_STREAM, NO_FLAGS, b"body"))
                .await?;
            // HPACK literal without indexing, new name "x", value "y".
            peer.write_all(&frame(
                FrameType::Headers,
                NEXT_STREAM,
                END_HEADERS | END_STREAM,
                &[0, 1, b'x', 1, b'y'],
            ))
            .await?;
            let reset = frame(
                FrameType::RstStream,
                FIRST_STREAM,
                NO_FLAGS,
                &(ErrorCode::Cancel as u32).to_be_bytes(),
            );
            let credit = window_update(CONNECTION, 4);
            let mut saw_reset = false;
            let mut saw_credit = false;
            while !(saw_reset && saw_credit) {
                let packet = read_wire_frame(&mut peer).await?;
                if packet == reset {
                    saw_reset = true;
                } else if packet == credit {
                    saw_credit = true;
                } else {
                    assert_eq!(packet, window_update(NEXT_STREAM, 4));
                }
            }

            let ping = [7; 8];
            peer.write_all(&frame(FrameType::Ping, CONNECTION, NO_FLAGS, &ping))
                .await?;
            assert_eq!(
                read_wire_frame(&mut peer).await?,
                frame(FrameType::Ping, CONNECTION, Flags::ACK, &ping)
            );
            peer.shutdown().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handler = async {
            let first = incoming.accept().await.ok_or("missing first request")?;
            assert_eq!(first.head.stream_id, FIRST_STREAM);
            drop(first);
            let mut next_request = incoming.accept().await.ok_or("missing next request")?;
            let next = &mut next_request.body;
            assert!(next.trailers().is_none());
            let chunk = next
                .data()
                .await
                .map_err(|error| format!("{error:?}"))?
                .ok_or("missing chunk")?;
            assert_eq!(chunk.as_bytes(), b"body");
            drop(chunk);
            assert!(matches!(next.data().await, Ok(None)));
            let trailers = next.trailers().ok_or("missing trailers")?;
            assert_eq!(trailers.len(), 1);
            assert_eq!(trailers[0].name, "x");
            assert_eq!(trailers[0].value, "y");
            assert!(next.trailers().is_none());
            assert!(incoming.accept().await.is_none());
            assert!(matches!(next.data().await, Err(StreamError::Disconnected)));
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handler) = tokio::join!(connection.run(), client, handler);
        client?;
        handler?;
        assert!(matches!(
            driver,
            Err(Error::Read(
                embedded_io_async::ReadExactError::UnexpectedEof
            ))
        ));
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

#[tokio::test]
async fn response_handles_isolate_send_backpressure_and_reset()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::server::{self, StreamError};
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        let settings = encode_settings(&[Setting {
            id: SettingId::InitialWindowSize,
            value: 1,
        }]);
        peer.write_all(&frame(FrameType::Settings, 0, NO_FLAGS, &settings))
            .await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;

        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            for id in [FIRST_STREAM, NEXT_STREAM] {
                peer.write_all(&frame(
                    FrameType::Headers,
                    id,
                    END_HEADERS | END_STREAM,
                    REQUEST,
                ))
                .await?;
            }
            let mut data = Vec::new();
            while data.len() < 3 {
                let packet = read_wire_frame(&mut peer).await?;
                if packet[3] == FrameType::Data as u8 {
                    data.push(packet);
                }
            }
            assert_eq!(
                data,
                [
                    frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"a"),
                    frame(FrameType::Data, NEXT_STREAM, NO_FLAGS, b"z"),
                    frame(FrameType::Data, NEXT_STREAM, END_STREAM, &[]),
                ]
            );
            peer.write_all(&frame(
                FrameType::RstStream,
                FIRST_STREAM,
                NO_FLAGS,
                &(ErrorCode::Cancel as u32).to_be_bytes(),
            ))
            .await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handlers = async {
            let headers = || {
                vec![HeaderField {
                    name: ":status".into(),
                    value: "200".into(),
                }]
            };
            let mut first = incoming
                .accept()
                .await
                .ok_or("missing first request")?
                .respond
                .send_response(headers())
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(
                first
                    .send_data(b"ab")
                    .await
                    .map_err(|error| format!("{error:?}"))?,
                2
            );
            let mut next = incoming
                .accept()
                .await
                .ok_or("missing second request")?
                .respond
                .send_response(headers())
                .map_err(|error| format!("{error:?}"))?;
            let (first_result, next_result) = tokio::join!(first.send_data(b"more"), async {
                assert_eq!(next.send_data(b"z").await?, 1);
                next.finish(Vec::new()).await
            });
            assert_eq!(
                first_result,
                Err(StreamError::Reset(ErrorCode::Cancel as u32))
            );
            assert_eq!(next_result, Ok(()));
            drop(incoming);
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handlers) = tokio::join!(connection.run(), client, handlers);
        client?;
        handlers?;
        driver.map_err(describe_transport_error)?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

#[tokio::test]
async fn response_finish_waits_for_final_data_and_sends_trailers()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::server;
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        let settings = encode_settings(&[Setting {
            id: SettingId::InitialWindowSize,
            value: 1,
        }]);
        peer.write_all(&frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings))
            .await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;
        let (finished, mut completion) = tokio::sync::oneshot::channel();
        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            peer.write_all(&frame(
                FrameType::Headers,
                FIRST_STREAM,
                END_HEADERS | END_STREAM,
                REQUEST,
            ))
            .await?;
            let head = read_wire_frame(&mut peer).await?;
            assert_eq!(head[3], FrameType::Headers as u8);
            assert_eq!(
                read_wire_frame(&mut peer).await?,
                frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"a")
            );
            assert!(matches!(
                completion.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            peer.write_all(&window_update(FIRST_STREAM, 1)).await?;
            completion.await?;
            assert_eq!(
                read_wire_frame(&mut peer).await?,
                frame(FrameType::Data, FIRST_STREAM, NO_FLAGS, b"b")
            );
            let trailers = read_wire_frame(&mut peer).await?;
            assert_eq!(trailers[3], FrameType::Headers as u8);
            assert_eq!(trailers[4], END_HEADERS | END_STREAM);
            assert_eq!(trailers[5..9], FIRST_STREAM.to_be_bytes());
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handler = async {
            let request = incoming.accept().await.ok_or("missing request")?;
            let mut send = request
                .respond
                .send_response(vec![HeaderField {
                    name: ":status".into(),
                    value: "200".into(),
                }])
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(
                send.send_data(b"ab")
                    .await
                    .map_err(|error| format!("{error:?}"))?,
                2
            );
            send.finish(vec![HeaderField {
                name: "grpc-status".into(),
                value: "0".into(),
            }])
            .await
            .map_err(|error| format!("{error:?}"))?;
            finished.send(()).map_err(|_| "peer exited")?;
            drop(incoming);
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handler) = tokio::join!(connection.run(), client, handler);
        client?;
        handler?;
        driver.map_err(describe_transport_error)?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

#[tokio::test]
async fn disconnect_wakes_a_response_writer_waiting_for_capacity()
-> Result<(), Box<dyn std::error::Error>> {
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        task::Poll,
    };
    use embedded_http2::server::{self, StreamError};
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        let settings = encode_settings(&[Setting {
            id: SettingId::InitialWindowSize,
            value: 0,
        }]);
        peer.write_all(&frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings))
            .await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;
        let (waiting, writer_polled) = tokio::sync::oneshot::channel();
        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            peer.write_all(&frame(
                FrameType::Headers,
                FIRST_STREAM,
                END_HEADERS | END_STREAM,
                REQUEST,
            ))
            .await?;
            writer_polled.await?;
            peer.shutdown().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handler = async {
            let request = incoming.accept().await.ok_or("missing request")?;
            let mut send = request
                .respond
                .send_response(vec![HeaderField {
                    name: ":status".into(),
                    value: "200".into(),
                }])
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(
                send.send_data(b"queued")
                    .await
                    .map_err(|error| format!("{error:?}"))?,
                6
            );
            let mut blocked = pin!(send.send_data(b"blocked"));
            poll_fn(|cx| {
                assert!(blocked.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            waiting.send(()).map_err(|_| "peer exited")?;
            assert_eq!(blocked.await, Err(StreamError::Disconnected));
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handler) = tokio::join!(connection.run(), client, handler);
        client?;
        handler?;
        assert!(matches!(
            driver,
            Err(Error::Read(
                embedded_io_async::ReadExactError::UnexpectedEof
            ))
        ));
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

#[tokio::test]
async fn response_header_limits_reset_only_the_affected_stream()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::server::{self, StreamError};
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(5), async {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        let settings = encode_settings(&[Setting {
            id: SettingId::MaxHeaderListSize,
            value: 64,
        }]);
        peer.write_all(&frame(FrameType::Settings, CONNECTION, NO_FLAGS, &settings))
            .await?;
        let (connection, mut incoming) = server::handshake(FromTokio::new(socket))
            .await
            .map_err(describe_transport_error)?;
        let client = async {
            let _settings = read_wire_frame(&mut peer).await?;
            let _ack = read_wire_frame(&mut peer).await?;
            for id in [1, 3] {
                peer.write_all(&frame(
                    FrameType::Headers,
                    id,
                    END_HEADERS | END_STREAM,
                    REQUEST,
                ))
                .await?;
            }
            for (id, code) in [(1, ErrorCode::Cancel), (3, ErrorCode::InternalError)] {
                assert_eq!(
                    read_wire_frame(&mut peer).await?,
                    frame(
                        FrameType::RstStream,
                        id,
                        NO_FLAGS,
                        &(code as u32).to_be_bytes()
                    )
                );
            }
            peer.write_all(&frame(
                FrameType::Headers,
                5,
                END_HEADERS | END_STREAM,
                REQUEST,
            ))
            .await?;
            let head = read_wire_frame(&mut peer).await?;
            assert_eq!(head[3], FrameType::Headers as u8);
            assert_eq!(head[5..9], 5_u32.to_be_bytes());
            assert_eq!(
                read_wire_frame(&mut peer).await?,
                frame(FrameType::Data, 5, END_STREAM, &[])
            );
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let handler = async {
            let header = |value: &str| HeaderField {
                name: "x".into(),
                value: value.into(),
            };
            let request = incoming.accept().await.ok_or("missing first request")?;
            assert!(matches!(
                request
                    .respond
                    .send_response(vec![header(&"x".repeat(4096))]),
                Err(StreamError::HeaderLimit)
            ));
            let request = incoming.accept().await.ok_or("missing second request")?;
            let mut send = request
                .respond
                .send_response(vec![header(&"x".repeat(64))])
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(
                send.send_data(b"body").await,
                Err(StreamError::Reset(ErrorCode::InternalError as u32))
            );
            let request = incoming.accept().await.ok_or("missing third request")?;
            let send = request
                .respond
                .send_response(vec![HeaderField {
                    name: ":status".into(),
                    value: "200".into(),
                }])
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(send.finish(Vec::new()).await, Ok(()));
            drop(incoming);
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let (driver, client, handler) = tokio::join!(connection.run(), client, handler);
        client?;
        handler?;
        driver.map_err(describe_transport_error)?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}
