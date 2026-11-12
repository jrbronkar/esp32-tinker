use alloc::vec::Vec;

use zerodds_hpack::{Decoder, HeaderField, decode_integer};

pub(crate) const MAX_BLOCK: usize = 4096;
pub(crate) const MAX_LIST: usize = 4096;
const MAX_TABLE: usize = 4096;

/// Check the decoded size after each HPACK field. An index can reference a
/// large table entry, so the compressed block size alone cannot bound memory.
pub(crate) fn decode(decoder: &mut Decoder, mut block: &[u8]) -> Option<Vec<HeaderField>> {
    let mut fields = Vec::new();
    let mut size = 0;
    while let Some(&first) = block.first() {
        // The high bits select the HPACK representation. The remaining bits
        // in this byte start its integer. See RFC 7541 sections 5.1 and 6.
        // https://www.rfc-editor.org/rfc/rfc7541.html#section-6
        let prefix = match first {
            0x80..=0xff => 7, // 1: indexed field
            0x40..=0x7f => 6, // 01: literal with incremental indexing
            0x20..=0x3f => 5, // 001: dynamic table size update
            _ => 4,           // 0000 / 0001: literal without / never indexed
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
