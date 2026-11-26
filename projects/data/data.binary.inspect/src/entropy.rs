//! Shannon entropy over byte ranges and sliding windows.
//!
//! Products that flag packed sections, encrypted blobs or compressed payloads
//! all recompute entropy. This module owns the counting, the unit (bits per
//! byte) and the windowing rules.

use crate::Result;
use serde::{Deserialize, Serialize};

/// Shannon entropy of `bytes` in bits per byte.
///
/// Empty input reports `0.0`; a constant buffer reports `0.0`; a buffer that
/// uses every byte value equally reports `8.0`.
pub fn shannon(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let total = bytes.len() as f64;
    let mut entropy = 0.0;
    for count in counts {
        if count == 0 {
            continue;
        }
        let probability = count as f64 / total;
        entropy -= probability * probability.log2();
    }
    entropy
}

/// Shannon entropy of the window `offset..offset + length`.
///
/// `length` defaults to the rest of the buffer. The window must lie inside
/// `bytes`; otherwise the returned error carries the stable `binary/range` code.
pub fn range(bytes: &[u8], offset: u64, length: Option<u64>) -> Result<f64> {
    Ok(shannon(crate::dump::window(bytes, offset, length)?))
}

/// One window of an [`EntropySeries`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntropyWindow {
    /// Offset of the window's first byte, counted from the start of the buffer.
    pub offset: u64,
    /// Window size in bytes.
    pub length: u64,
    /// Shannon entropy in bits per byte.
    pub entropy: f64,
}

/// Bounded windowed entropy series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntropySeries {
    /// Window size in bytes.
    pub window: usize,
    /// Distance between consecutive window starts, in bytes.
    pub step: usize,
    /// Windows that survived the limit.
    pub windows: Vec<EntropyWindow>,
    /// Number of windows before the limit was applied.
    pub total: usize,
    /// Whether the limit removed windows.
    pub truncated: bool,
}

/// Computes the entropy of every `window`-byte slice of the range
/// `offset..offset + length`, advancing by `step` bytes.
///
/// Only complete windows are reported. `limit` bounds [`EntropySeries::windows`];
/// a zero `window` or `step` is rejected with the stable `binary/range` code.
pub fn series(bytes: &[u8], offset: u64, length: Option<u64>, window: usize, step: usize, limit: usize) -> Result<EntropySeries> {
    if window == 0 {
        return Err(crate::InspectError::range("entropy window must be at least one byte"));
    }
    if step == 0 {
        return Err(crate::InspectError::range("entropy step must be at least one byte"));
    }
    let data = crate::dump::window(bytes, offset, length)?;
    let mut windows = Vec::new();
    let mut total = 0usize;
    let mut start = 0usize;
    while start + window <= data.len() {
        total += 1;
        if windows.len() < limit {
            windows.push(EntropyWindow {
                offset: offset + start as u64,
                length: window as u64,
                entropy: shannon(&data[start..start + window]),
            });
        }
        start += step;
    }
    let truncated = total > windows.len();
    Ok(EntropySeries { window, step, windows, total, truncated })
}
