use alloc::vec::Vec;

use zerodds_hpack::{Decoder, HeaderField, decode_integer};

pub(crate) const MAX_BLOCK: usize = 4096;
pub(crate) const MAX_LIST: usize = 4096;
const MAX_TABLE: usize = 4096;

/// Decode one representation at a time so indexed fields cannot expand an
/// entire block into a large intermediate allocation.
pub(crate) fn decode(decoder: &mut Decoder, mut block: &[u8]) -> Option<Vec<HeaderField>> {
    let mut fields = Vec::new();
    let mut size = 0;
    while let Some(&first) = block.first() {
        let prefix = match first {
            0x80..=0xff => 7,
            0x40..=0x7f => 6,
            0x20..=0x3f => 5,
            _ => 4,
        };
        let (index, mut len) = decode_integer(block, prefix).ok()?;
        match prefix {
            5 if !fields.is_empty() || index > MAX_TABLE as u64 => return None,
            4 | 6 => {
                if index == 0 {
                    len += string_len(block.get(len..)?)?;
                }
                len += string_len(block.get(len..)?)?;
            }
            _ => {}
        }
        for field in decoder.decode(block.get(..len)?).ok()? {
            size += field.size();
            if size > MAX_LIST {
                return None;
            }
            fields.push(field);
        }
        block = block.get(len..)?;
    }
    Some(fields)
}

fn string_len(bytes: &[u8]) -> Option<usize> {
    let (len, prefix) = decode_integer(bytes, 7).ok()?;
    let end = usize::try_from(len).ok()?.checked_add(prefix)?;
    (end <= bytes.len()).then_some(end)
}
