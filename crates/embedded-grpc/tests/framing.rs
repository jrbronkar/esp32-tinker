use embedded_grpc::{MessageDecoder, Status};
use ota_protocol::StatusRequest;

const MESSAGE: [u8; 7] = [0, 0, 0, 0, 2, 8, 1];
const EMPTY: [u8; 5] = [0; 5];

#[test]
fn decodes_fragmented_and_coalesced_messages() -> Result<(), Status> {
    for split in 1..MESSAGE.len() {
        let mut decoder = MessageDecoder::<StatusRequest, 2>::default();
        let mut prefix = &MESSAGE[..split];
        assert!(decoder.push(&mut prefix)?.is_none());
        assert!(prefix.is_empty());
        let tail = [&MESSAGE[split..], &EMPTY].concat();
        let mut tail = tail.as_slice();
        assert_eq!(
            decoder.push(&mut tail)?,
            Some(StatusRequest {
                include_timings: true,
                ..Default::default()
            })
        );
        assert_eq!(decoder.push(&mut tail)?, Some(StatusRequest::default()));
        assert!(tail.is_empty());
        decoder.finish()?;
    }
    Ok(())
}

#[test]
fn rejects_invalid_and_incomplete_messages() -> Result<(), Status> {
    for (mut bytes, status) in [
        (&[1, 0, 0, 0, 0][..], Status::Unimplemented),
        (&[2, 0, 0, 0, 0][..], Status::InvalidArgument),
        (&[0, 0, 0, 0, 3][..], Status::ResourceExhausted),
        (&[0, 0, 0, 0, 1, 0xff][..], Status::InvalidArgument),
    ] {
        let mut decoder = MessageDecoder::<StatusRequest, 2>::default();
        assert_eq!(decoder.push(&mut bytes), Err(status));
    }
    for count in 1..MESSAGE.len() {
        let mut decoder = MessageDecoder::<StatusRequest, 2>::default();
        assert!(decoder.push(&mut &MESSAGE[..count])?.is_none());
        assert_eq!(decoder.finish(), Err(Status::InvalidArgument));
    }
    Ok(())
}

#[test]
fn decodes_byte_at_a_time_with_empty_input_between_reads() -> Result<(), Status> {
    let mut decoder = MessageDecoder::<StatusRequest, 2>::default();
    for (index, byte) in MESSAGE.iter().enumerate() {
        assert!(decoder.push(&mut &[][..])?.is_none());
        let mut input = core::slice::from_ref(byte);
        let decoded = decoder.push(&mut input)?;
        assert!(input.is_empty());
        if index + 1 == MESSAGE.len() {
            assert!(decoded.is_some_and(|request| request.include_timings));
        } else {
            assert!(decoded.is_none());
        }
    }
    assert!(decoder.push(&mut &[][..])?.is_none());
    decoder.finish()
}

#[test]
fn enforces_big_endian_length_before_consuming_payload() -> Result<(), Status> {
    // 128 copies of a two-byte protobuf field make a valid 256-byte message.
    let payload = [8, 1].repeat(128);
    let mut wire = vec![0, 0, 0, 1, 0];
    wire.extend_from_slice(&payload);

    let mut limited = MessageDecoder::<StatusRequest, 255>::default();
    let mut input = wire.as_slice();
    assert_eq!(limited.push(&mut input), Err(Status::ResourceExhausted));
    assert_eq!(input, payload.as_slice());

    let mut exact = MessageDecoder::<StatusRequest, 256>::default();
    let mut input = wire.as_slice();
    assert!(
        exact
            .push(&mut input)?
            .is_some_and(|request| request.include_timings)
    );
    assert!(input.is_empty());
    exact.finish()
}

#[test]
fn rejects_fragmented_malformed_payload_and_preserves_trailing_bytes() -> Result<(), Status> {
    let mut decoder = MessageDecoder::<StatusRequest, 2>::default();
    assert!(decoder.push(&mut &[0, 0, 0, 0, 2, 0xff][..])?.is_none());
    let mut input = &[0xff, 0, 0, 0, 0, 0][..];
    assert_eq!(decoder.push(&mut input), Err(Status::InvalidArgument));
    assert_eq!(input, EMPTY.as_slice());
    Ok(())
}
