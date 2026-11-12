use core::convert::Infallible;

use crate::error::*;
use crate::sync::DecodedFrame;

/// Progress reported with [`DecodeError::InvalidFrame`](crate::DecodeError::InvalidFrame).
///
/// Describes how far decoding got before a delimiter arrived inside an
/// incomplete COBS block.
///
/// The default value contains zero counts and `frame_len: None`.
///
/// The `serde` feature enables serialization and deserialization. The `defmt`
/// feature enables compact diagnostic formatting.
#[must_use = "Inspect consumed input and frame completion before processing the next chunk"]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct DecodeProgress {
    /// Number of encoded bytes processed, including zero padding and the
    /// invalidating delimiter.
    ///
    /// The next frame starts at `source[consumed as usize..]`.
    pub consumed: u64,

    /// Number of decoded bytes written to the destination before the frame
    /// was found invalid.
    ///
    /// Includes reconstructed payload zeros. The bytes remain in the
    /// destination.
    pub written: u64,

    /// Decoded length of a frame completed by a delimiter.
    ///
    /// Always `None` in an [`InvalidFrame`](crate::DecodeError::InvalidFrame)
    /// error.
    pub frame_len: Option<u64>,
}

#[derive(Clone, Debug)]
struct DecoderState {
    next_sentinel_idx: u8,
    current_block_is_full: bool,
}

enum PushResult {
    AddSingle,
    WriteSentinel,
    InvalidFrame,
    EndOfFrame,
    Skip,
}

impl DecoderState {
    fn new() -> Self {
        Self {
            next_sentinel_idx: 0,
            current_block_is_full: false,
        }
    }
    #[inline]
    fn push(&mut self, byte: u8) -> PushResult {
        if byte == 0 {
            if self.next_sentinel_idx > 1 {
                self.next_sentinel_idx = 0;
                return PushResult::InvalidFrame;
            }
            self.next_sentinel_idx = 0;
            return PushResult::EndOfFrame;
        }
        if self.next_sentinel_idx <= 1 {
            let ret = if self.next_sentinel_idx == 0 || self.current_block_is_full {
                PushResult::Skip
            } else {
                PushResult::WriteSentinel
            };
            self.current_block_is_full = byte == 0xFF;
            self.next_sentinel_idx = byte;
            ret
        } else {
            self.next_sentinel_idx -= 1;
            PushResult::AddSingle
        }
    }
}

/// Decodes the first frame of `source` into `dest`.
pub(crate) fn decode_slice(
    source: &[u8],
    dest: &mut [u8],
) -> Result<DecodedFrame, DecodeError<Infallible, SeekableError>> {
    let mut state = DecoderState::new();
    let mut started = false;
    let mut written = 0;
    for (idx, &byte) in source.iter().enumerate() {
        if !started && byte == 0 {
            continue;
        }
        started = true;
        let value = match state.push(byte) {
            PushResult::Skip => continue,
            PushResult::AddSingle => byte,
            PushResult::WriteSentinel => 0,
            PushResult::EndOfFrame => {
                return Ok(DecodedFrame {
                    len: written,
                    consumed: idx + 1,
                });
            }
            PushResult::InvalidFrame => {
                return Err(DecodeError::InvalidFrame(DecodeProgress {
                    consumed: idx as u64 + 1,
                    written: written as u64,
                    frame_len: None,
                }));
            }
        };
        *dest
            .get_mut(written)
            .ok_or(DecodeError::Destination(SeekableError::OutOfBounds))? = value;
        written += 1;
    }
    if state.next_sentinel_idx > 1 {
        return Err(DecodeError::UnexpectedSourceEof);
    }
    if !started {
        return Err(DecodeError::EmptyFrame);
    }
    Ok(DecodedFrame {
        len: written,
        consumed: source.len(),
    })
}
