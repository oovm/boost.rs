//! Hex-dump rendering over an in-memory byte window.
//!
//! [`hexdump`] is the single implementation behind
//! [`crate::BinarySession::hexdump`]; the session only supplies its bytes and
//! default length.

use crate::{InspectError, Result};

/// Slices the window `offset..offset + length` out of `bytes`.
///
/// `length` defaults to the rest of the slice. The window must lie inside
/// `bytes`; otherwise the returned error carries the stable `binary/range` code.
pub fn window<'a>(bytes: &'a [u8], offset: u64, length: Option<u64>) -> Result<&'a [u8]> {
    let total = bytes.len() as u64;
    let end = match length {
        Some(length) => offset.checked_add(length).ok_or_else(|| InspectError::range(format!("window at 0x{offset:x} overflows")))?,
        None => total,
    };
    if offset > total || end > total {
        return Err(InspectError::range(format!("window of 0x{offset:x}..0x{end:x} is outside the {total} byte buffer")));
    }
    Ok(&bytes[offset as usize..end as usize])
}

/// Renders `length` bytes at `offset` as a hex dump.
///
/// Each row shows 16 bytes, an extra gap after the eighth byte, and an ASCII
/// gutter where non-printable bytes become `.`. Rows are labelled with the byte
/// offset counted from `bytes`, so a non-zero `offset` keeps 16-byte alignment.
pub fn hexdump(bytes: &[u8], offset: u64, length: Option<u64>) -> Result<String> {
    Ok(render(window(bytes, offset, length)?, offset))
}

/// Renders already-selected bytes, labelling the first row with `offset`.
pub fn render(bytes: &[u8], offset: u64) -> String {
    let mut output = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let row_offset = offset + row as u64 * 16;
        output.push_str(&format!("{row_offset:08x}  "));
        for index in 0..16 {
            if index == 8 {
                output.push(' ');
            }
            match chunk.get(index) {
                Some(byte) => output.push_str(&format!("{byte:02x} ")),
                None => output.push_str("   "),
            }
        }
        output.push_str(" |");
        for byte in chunk {
            output.push(if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' });
        }
        output.push_str("|\n");
    }
    output
}
