//! Byte-level comparison of two in-memory buffers.
//!
//! Products that compare a patched file with its original, or one capture with
//! another, repeat the same prefix/suffix trimming and changed-range grouping.
//! This module performs the comparison once and returns a bounded description
//! of every changed range, so callers only render the result.

use serde::{Deserialize, Serialize};

/// Bounds applied to one [`diff`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffOptions {
    /// Maximum number of changed ranges to return.
    pub max_ranges: usize,
    /// Maximum number of preview bytes kept for each side of a range.
    pub preview_bytes: usize,
}

impl DiffOptions {
    /// Builds options that keep `max_ranges` ranges with `preview_bytes` of preview.
    pub fn new(max_ranges: usize, preview_bytes: usize) -> Self {
        Self { max_ranges, preview_bytes }
    }
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self { max_ranges: 64, preview_bytes: 16 }
    }
}

/// One run of byte positions that differ between the two buffers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedRange {
    /// Offset of the first differing position, counted from the start of both buffers.
    pub offset: u64,
    /// Number of bytes the left buffer contributes to this range; `0` for a pure insertion.
    pub left_length: u64,
    /// Number of bytes the right buffer contributes to this range; `0` for a pure deletion.
    pub right_length: u64,
    /// Printable preview of the left side, with non-printable bytes shown as `.`.
    pub left_preview: String,
    /// Printable preview of the right side, with non-printable bytes shown as `.`.
    pub right_preview: String,
}

/// Aggregate facts about one comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    /// Length of the left buffer.
    pub left_length: u64,
    /// Length of the right buffer.
    pub right_length: u64,
    /// Length of the common leading run.
    pub equal_prefix: u64,
    /// Length of the common trailing run; never overlaps [`Self::equal_prefix`].
    pub equal_suffix: u64,
    /// Number of byte positions inside changed ranges.
    pub changed_bytes: u64,
}

/// Bounded result of [`diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport {
    /// Prefix, suffix and changed-byte counts.
    pub summary: DiffSummary,
    /// Ranges that survived [`DiffOptions::max_ranges`].
    pub ranges: Vec<ChangedRange>,
    /// Number of changed ranges before the limit was applied.
    pub total: usize,
    /// Whether the limit removed ranges.
    pub truncated: bool,
}

/// Compares two byte buffers and reports where they differ.
///
/// The comparison trims the common prefix and suffix, then walks the remaining
/// middle by position, grouping adjacent differing positions into one range. A
/// shorter side contributes `0` bytes to the tail of a range, which is how an
/// insertion or deletion appears. The operation is total: any budget only bounds
/// the returned output, never the reported summary.
pub fn diff(left: &[u8], right: &[u8], options: &DiffOptions) -> DiffReport {
    let common = left.len().min(right.len());
    let mut prefix = 0usize;
    while prefix < common && left[prefix] == right[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < common - prefix && left[left.len() - 1 - suffix] == right[right.len() - 1 - suffix] {
        suffix += 1;
    }
    let left_middle = &left[prefix..left.len() - suffix];
    let right_middle = &right[prefix..right.len() - suffix];
    let span = left_middle.len().max(right_middle.len());
    let mut ranges = Vec::new();
    let mut total = 0usize;
    let mut changed_bytes = 0u64;
    let mut index = 0usize;
    while index < span {
        if byte_at(left_middle, index) == byte_at(right_middle, index) {
            index += 1;
            continue;
        }
        let start = index;
        while index < span && byte_at(left_middle, index) != byte_at(right_middle, index) {
            index += 1;
        }
        total += 1;
        changed_bytes += (index - start) as u64;
        if ranges.len() < options.max_ranges {
            let left_slice = slice_of(left_middle, start, index);
            let right_slice = slice_of(right_middle, start, index);
            ranges.push(ChangedRange {
                offset: (prefix + start) as u64,
                left_length: left_slice.len() as u64,
                right_length: right_slice.len() as u64,
                left_preview: printable_preview(left_slice, options.preview_bytes),
                right_preview: printable_preview(right_slice, options.preview_bytes),
            });
        }
    }
    let truncated = total > ranges.len();
    DiffReport {
        summary: DiffSummary {
            left_length: left.len() as u64,
            right_length: right.len() as u64,
            equal_prefix: prefix as u64,
            equal_suffix: suffix as u64,
            changed_bytes,
        },
        ranges,
        total,
        truncated,
    }
}

fn byte_at(bytes: &[u8], index: usize) -> Option<u8> {
    bytes.get(index).copied()
}

fn slice_of(bytes: &[u8], start: usize, end: usize) -> &[u8] {
    let start = start.min(bytes.len());
    let end = end.min(bytes.len()).max(start);
    &bytes[start..end]
}

fn printable_preview(bytes: &[u8], limit: usize) -> String {
    let mut text = String::new();
    for byte in bytes.iter().take(limit) {
        text.push(if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' });
    }
    text
}
