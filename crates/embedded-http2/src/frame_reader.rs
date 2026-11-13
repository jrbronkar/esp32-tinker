use alloc::vec::Vec;
use embedded_io_async::{Read, ReadExactError};
use zerodds_http2::{
    Flags, Http2Error, decode_frame,
    frame::{DEFAULT_MAX_FRAME_SIZE, FRAME_HEADER_LEN},
};

use crate::{Error, Frame};

// RFC 9113 section 4.1 defines a 31-bit stream identifier.
const STREAM_ID_MASK: u32 = 0x7fff_ffff;

/// Keeps completed reads across polls, including partial frame headers.
/// Dropping a pending read preserves this progress if the underlying
/// `Read::read` implementation is cancellation safe.
#[derive(Default)]
pub(super) struct FrameReader {
    filled: usize,
}

impl FrameReader {
    pub(super) async fn read<Io: Read>(
        &mut self,
        io: &mut Io,
        buffer: &mut Vec<u8>,
    ) -> Result<Frame, Error<Io::Error>> {
        if self.filled < FRAME_HEADER_LEN {
            buffer.resize(FRAME_HEADER_LEN, 0);
            self.fill(io, buffer).await?;
        }

        let len = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
        if len > DEFAULT_MAX_FRAME_SIZE {
            return Err(Http2Error::FrameTooLarge {
                got: len,
                max: DEFAULT_MAX_FRAME_SIZE,
            }
            .into());
        }
        buffer.resize(FRAME_HEADER_LEN + len as usize, 0);
        self.fill(io, buffer).await?;
        let (kind, flags, stream_id) = match decode_frame(buffer, DEFAULT_MAX_FRAME_SIZE) {
            Ok((frame, _)) => (
                frame.header.frame_type as u8,
                frame.header.flags,
                frame.header.stream_id,
            ),
            Err(Http2Error::UnknownFrameType(kind)) => {
                let id = u32::from_be_bytes([buffer[5], buffer[6], buffer[7], buffer[8]])
                    & STREAM_ID_MASK;
                (kind, Flags(buffer[4]), id)
            }
            Err(error) => return Err(error.into()),
        };
        self.filled = 0;
        buffer.drain(..FRAME_HEADER_LEN);
        Ok(Frame {
            kind,
            flags,
            stream_id,
        })
    }

    async fn fill<Io: Read>(
        &mut self,
        io: &mut Io,
        buffer: &mut [u8],
    ) -> Result<(), Error<Io::Error>> {
        while self.filled < buffer.len() {
            let read = io
                .read(&mut buffer[self.filled..])
                .await
                .map_err(|error| Error::Read(ReadExactError::Other(error)))?;
            if read == 0 {
                return Err(Error::Read(ReadExactError::UnexpectedEof));
            }
            self.filled += read;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        task::Poll,
    };
    use embedded_io_adapters::tokio_1::FromTokio;
    use tokio::io::{AsyncWriteExt, duplex};
    use zerodds_http2::FrameType;

    use super::*;

    #[tokio::test]
    async fn resumes_after_interruptions_in_the_header_and_payload()
    -> Result<(), Box<dyn core::error::Error>> {
        let (socket, mut peer) = duplex(64);
        let mut io = FromTokio::new(socket);
        let mut buffer = Vec::new();
        let mut reader = FrameReader::default();
        let frame = [
            0,
            0,
            3,
            FrameType::Data as u8,
            Flags::END_STREAM,
            0,
            0,
            0,
            1,
            b'a',
            b'b',
            b'c',
        ];

        let mut start = 0;
        for end in [4, FRAME_HEADER_LEN + 1] {
            peer.write_all(&frame[start..end]).await?;
            let mut read = pin!(reader.read(&mut io, &mut buffer));
            poll_fn(|cx| {
                assert!(read.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            start = end;
        }
        peer.write_all(&frame[start..]).await?;
        peer.write_all(&frame).await?;

        for _ in 0..2 {
            let received = reader
                .read(&mut io, &mut buffer)
                .await
                .map_err(|error| alloc::format!("{error:?}"))?;
            assert_eq!(received.kind, FrameType::Data as u8);
            assert_eq!(received.stream_id, 1);
            assert!(received.flags.has(Flags::END_STREAM));
            assert_eq!(buffer, b"abc");
        }
        Ok(())
    }
}
