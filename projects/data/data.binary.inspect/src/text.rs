//! Printable-string extraction from a byte window.
//!
//! ASCII extraction reuses the scanner behind [`crate::BinarySession::strings`];
//! this module adds the UTF-16LE mode that the Windows tools repeat, plus an
//! explicit window and hit budget.

use crate::{Result, StringHit, StringsReport, scan_strings};
use serde::{Deserialize, Serialize};

/// How [`extract`] reads printable runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextMode {
    /// One printable byte per character.
    Ascii,
    /// Two little-endian bytes per character, as stored by Windows wide strings.
    Utf16Le,
}

impl TextMode {
    /// Stable lower-case name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Utf16Le => "utf16le",
        }
    }
}

/// Inputs of one [`extract`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextOptions {
    /// Unit width used to read printable runs.
    pub mode: TextMode,
    /// Minimum run length; values below `1` are treated as `1`.
    pub min_length: usize,
    /// Maximum number of hits; `null` means unlimited.
    pub limit: Option<usize>,
    /// First byte of the window, counted from the start of the buffer.
    pub offset: u64,
    /// Window size in bytes; `null` runs to the end of the buffer.
    pub length: Option<u64>,
}

impl TextOptions {
    /// Builds bounded options over the whole buffer.
    pub fn new(mode: TextMode, min_length: usize) -> Self {
        Self { mode, min_length, limit: Some(100), offset: 0, length: None }
    }
}

/// Extracts printable runs from one window of `bytes`.
///
/// Hits carry absolute offsets, so a non-zero [`TextOptions::offset`] is added to
/// every reported offset. In UTF-16LE mode a trailing odd byte is ignored.
/// The window must lie inside `bytes`; otherwise the returned error carries the
/// stable `binary/range` code.
pub fn extract(bytes: &[u8], options: &TextOptions) -> Result<StringsReport> {
    let data = crate::dump::window(bytes, options.offset, options.length)?;
    let minimum = options.min_length.max(1);
    let mut hits = match options.mode {
        TextMode::Ascii => scan_strings(data, minimum),
        TextMode::Utf16Le => scan_utf16(data, minimum),
    };
    for hit in &mut hits {
        hit.offset += options.offset;
    }
    let total = hits.len();
    let limit = options.limit.unwrap_or(usize::MAX);
    let truncated = total > limit;
    hits.truncate(limit);
    Ok(StringsReport { hits, total, truncated })
}

fn scan_utf16(bytes: &[u8], min_len: usize) -> Vec<StringHit> {
    let minimum = min_len.max(1);
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    let mut hits = Vec::new();
    let mut start: Option<usize> = None;
    for (index, unit) in units.iter().copied().enumerate() {
        let printable = (0x20..=0x7e).contains(&unit);
        match (printable, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                if index - begin >= minimum {
                    push_utf16_hit(&mut hits, begin, &units[begin..index]);
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start
        && units.len() - begin >= minimum
    {
        push_utf16_hit(&mut hits, begin, &units[begin..]);
    }
    hits
}

fn push_utf16_hit(hits: &mut Vec<StringHit>, offset: usize, units: &[u16]) {
    hits.push(StringHit { offset: (offset * 2) as u64, length: (units.len() * 2) as u64, value: String::from_utf16_lossy(units) });
}
