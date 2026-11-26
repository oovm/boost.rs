//! Literal byte-pattern and string search with explicit budgets.
//!
//! [`find`] returns offsets only, so a caller that walks a whole image cannot
//! build an unbounded result. It reuses [`crate::SearchMode`] and the pattern
//! compiler behind [`crate::BinarySession::search`], adding a maximum hit count
//! and a maximum scanned span.

use crate::{Result, SearchMode, SearchOptions, build_needle, matches_at, preview};
use serde::{Deserialize, Serialize};

/// Inputs of one [`find`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindOptions {
    /// How the pattern is interpreted.
    pub mode: SearchMode,
    /// Whether ASCII letters must match case; hex bytes always match exactly.
    pub case_sensitive: bool,
    /// Maximum number of hits to return.
    pub max_hits: usize,
    /// Maximum number of bytes to scan from the start of the buffer; `null` scans everything.
    pub max_span: Option<u64>,
}

impl FindOptions {
    /// Builds case-insensitive options bounded to 100 hits over the whole buffer.
    pub fn new(mode: SearchMode) -> Self {
        Self { mode, case_sensitive: false, max_hits: 100, max_span: None }
    }
}

/// One match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHit {
    /// Offset of the first matched byte.
    pub offset: u64,
    /// Number of matched bytes.
    pub length: u64,
    /// Printable preview of the match.
    pub preview: String,
}

/// Bounded result of [`find`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindReport {
    /// Pattern kind that was used.
    pub mode: String,
    /// Hits that survived the limit.
    pub hits: Vec<FindHit>,
    /// Number of hits before the limit was applied.
    pub total: usize,
    /// Number of bytes that were scanned.
    pub scanned: u64,
    /// Whether the hit limit removed hits.
    pub truncated: bool,
    /// Whether [`FindOptions::max_span`] stopped the scan before the end of the buffer.
    pub span_limited: bool,
}

/// Searches `bytes` for a literal byte or string pattern.
///
/// The pattern is parsed by the same rules as
/// [`crate::SearchOptions`]: `mode` selects hex bytes, ASCII or UTF-16LE.
/// The scan stops after `max_span` bytes and keeps at most `max_hits` hits, while
/// `total` still counts every match inside the scanned span. An empty or
/// malformed pattern is rejected with the stable `binary/parse` code.
pub fn find(bytes: &[u8], pattern: &str, options: &FindOptions) -> Result<FindReport> {
    let search = SearchOptions {
        mode: options.mode,
        pattern: pattern.to_string(),
        case_sensitive: options.case_sensitive,
        whole_word: false,
        limit: None,
        start: None,
        end: None,
    };
    let needle = build_needle(&search)?;
    let scan = options.max_span.map(|span| span.min(bytes.len() as u64)).unwrap_or(bytes.len() as u64);
    let haystack = &bytes[..scan as usize];
    let mut hits = Vec::new();
    let mut total = 0usize;
    let mut index = 0usize;
    while index + needle.len() <= haystack.len() {
        if matches_at(haystack, index, &needle, &search) {
            total += 1;
            if hits.len() < options.max_hits {
                hits.push(FindHit {
                    offset: index as u64,
                    length: needle.len() as u64,
                    preview: preview(options.mode, &haystack[index..index + needle.len()]),
                });
            }
        }
        index += 1;
    }
    let truncated = total > hits.len();
    Ok(FindReport { mode: options.mode.as_str().to_string(), hits, total, scanned: scan, truncated, span_limited: scan < bytes.len() as u64 })
}
