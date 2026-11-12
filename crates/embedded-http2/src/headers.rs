use alloc::vec::Vec;

use zerodds_hpack::{Decoder, HeaderField, decode_integer};

// Local limits for encoded blocks and decoded header lists.
pub(crate) const MAX_BLOCK: usize = 4 * 1024;
pub(crate) const MAX_LIST: usize = 4 * 1024;
// HTTP/2's default SETTINGS_HEADER_TABLE_SIZE (RFC 9113 section 6.5.2).
const MAX_TABLE: usize = 4 * 1024;

const INDEXED_FIELD_BITS: u8 = 7;
const INCREMENTAL_INDEXING_BITS: u8 = 6;
const TABLE_SIZE_BITS: u8 = 5;
const LITERAL_BITS: u8 = 4;

// RFC 7541 section 6.3 encodes the new size after the 001 tag.
pub(crate) const ZERO_TABLE_SIZE_UPDATE: u8 = 0b0010_0000;

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
            0x80..=0xff => INDEXED_FIELD_BITS,        // 1: indexed field
            0x40..=0x7f => INCREMENTAL_INDEXING_BITS, // 01: literal with incremental indexing
            0x20..=0x3f => TABLE_SIZE_BITS,           // 001: dynamic table size update
            _ => LITERAL_BITS, // 0000 / 0001: literal without / never indexed
        };
        let (index, mut len) = decode_integer(block, prefix).ok()?;
        match prefix {
            TABLE_SIZE_BITS if !fields.is_empty() || index > MAX_TABLE as u64 => return None,
            LITERAL_BITS | INCREMENTAL_INDEXING_BITS => {
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
    // RFC 7541 section 5.2 reserves the high bit for the Huffman flag.
    const STRING_LENGTH_BITS: u8 = 7;
    let (len, prefix) = decode_integer(bytes, STRING_LENGTH_BITS).ok()?;
    let end = usize::try_from(len).ok()?.checked_add(prefix)?;
    (end <= bytes.len()).then_some(end)
}
