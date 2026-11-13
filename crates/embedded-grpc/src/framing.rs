//! Incremental decoding of identity-encoded gRPC messages.

use alloc::vec::Vec;
use core::mem;
use micropb::MessageDecode;

use crate::Status;

pub(crate) const UNCOMPRESSED: u8 = 0;
pub(crate) const ENVELOPE_LEN: usize = size_of::<u8>() + size_of::<u32>();

pub(crate) fn message_length<const MAX: usize>(
    header: [u8; ENVELOPE_LEN],
) -> Result<usize, Status> {
    let [compression, length @ ..] = header;
    match compression {
        UNCOMPRESSED => {}
        1 => return Err(Status::Unimplemented),
        _ => return Err(Status::InvalidArgument),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX {
        return Err(Status::ResourceExhausted);
    }
    Ok(length)
}

/// Decode a stream of gRPC messages across arbitrary HTTP/2 DATA boundaries.
/// The decoder retains at most one message of up to `MAX` payload bytes.
pub struct MessageDecoder<M, const MAX: usize> {
    header: [u8; ENVELOPE_LEN],
    header_len: usize,
    payload: Vec<u8>,
    message: core::marker::PhantomData<fn() -> M>,
}

impl<M, const MAX: usize> Default for MessageDecoder<M, MAX> {
    fn default() -> Self {
        Self {
            header: [0; ENVELOPE_LEN],
            header_len: 0,
            payload: Vec::new(),
            message: core::marker::PhantomData,
        }
    }
}

impl<M: MessageDecode + Default, const MAX: usize> MessageDecoder<M, MAX> {
    /// Consume bytes through the next complete message, leaving any subsequent
    /// bytes in `input`. A framing or decoding error terminates the RPC.
    pub fn push(&mut self, input: &mut &[u8]) -> Result<Option<M>, Status> {
        if self.header_len < ENVELOPE_LEN {
            let remaining = &mut self.header[self.header_len..];
            let count = remaining.len().min(input.len());
            let (prefix, rest) = input.split_at(count);
            remaining[..count].copy_from_slice(prefix);
            *input = rest;
            self.header_len += count;
            if self.header_len < ENVELOPE_LEN {
                return Ok(None);
            }
        }

        let length = message_length::<MAX>(self.header)?;
        if self.payload.is_empty() && input.len() >= length {
            let (payload, rest) = input.split_at(length);
            *input = rest;
            self.header_len = 0;
            return decode_message(payload).map(Some);
        }

        let count = (length - self.payload.len()).min(input.len());
        let (prefix, rest) = input.split_at(count);
        self.payload.extend_from_slice(prefix);
        *input = rest;
        if self.payload.len() < length {
            return Ok(None);
        }
        self.header_len = 0;
        decode_message(&mem::take(&mut self.payload)).map(Some)
    }

    /// Verify the request half-closed between messages.
    pub fn finish(self) -> Result<(), Status> {
        if self.header_len == 0 {
            Ok(())
        } else {
            Err(Status::InvalidArgument)
        }
    }
}

pub(crate) fn decode_message<M: MessageDecode + Default>(payload: &[u8]) -> Result<M, Status> {
    let mut message = M::default();
    message
        .decode_from_bytes(payload)
        .map_err(|_| Status::InvalidArgument)?;
    Ok(message)
}
