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
