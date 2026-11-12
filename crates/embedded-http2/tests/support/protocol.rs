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
    for invalid in invalid_frames {
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
        assert!(matches!(
            server.read_body(head.stream_id).await,
            Err(Error::Protocol)
        ));
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
