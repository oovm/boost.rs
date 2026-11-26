//! Tauri v2 embedded-asset extraction.
//!
//! Tauri v2 compiles its frontend into the executable as an `EmbeddedAssets`
//! map — a `phf::Map<&'static str, &'static [u8]>` whose values are the
//! `include_bytes!` output of the codegen step: one standalone brotli stream
//! (quality 9) per asset. In the analysed builds the map lowers to a single
//! interleaved stride-16 array of fat-pointer records,
//! `[key_ptr, key_len, blob_ptr, blob_len, key_ptr, ...]`, where the pointers
//! are PE virtual addresses.
//!
//! Carving bare brotli streams out of the whole file produces a flood of
//! false positives because the format carries no container length, so this
//! module instead walks the interleaved structure: a key record is a `(ptr,
//! len)` pair whose target is a printable `/…` path containing a dot, and a
//! blob record is a `(ptr, len)` pair whose target brotli-decodes while
//! consuming the whole declared length. The longest consecutive run of
//! complete `(key, blob)` pairs — at least two — is treated as the asset
//! table, which keeps the extraction free of false positives.

use std::path::Path;

use data_binary_pe::pe;
use data_compression::{Brotli, CodecLimits};
use serde::{Deserialize, Serialize};

use crate::{ContainerError, Result};

/// Asset limit used when the caller does not ask for one.
const DEFAULT_LIMIT: usize = 200;
/// Hard cap on the number of assets one scan returns.
const MAX_LIMIT: usize = 1000;
/// Number of decompressed bytes rendered into each preview.
const PREVIEW_BYTES: usize = 120;
/// Shortest asset key the record detector accepts.
const MIN_KEY_LEN: u64 = 4;
/// Longest asset key the record detector accepts.
const MAX_KEY_LEN: u64 = 128;
/// Byte stride of one `(ptr, len)` fat-pointer record.
const RECORD_STRIDE: usize = 16;
/// Byte stride of one interleaved `(key, blob)` entry.
const ENTRY_STRIDE: usize = 32;
/// Printable share a preview window must reach before it is rendered.
const MIN_PREVIEW_RATIO: f64 = 0.9;
/// Cap on the decoded size of one embedded asset.
const MAX_DECODED_ASSET: usize = 64 * 1024 * 1024;

/// Inputs of one [`analyze`] call.
///
/// Serialized as JSON with camelCase keys, so `{ "outputDir": "C:\\tmp\\app" }`
/// is the wire form the addon and the TypeScript client both use.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TauriAssetsOptions {
    /// Directory the decoded assets are written to, mirroring their key paths.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Maximum number of assets to return; defaults to 200 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One embedded asset recovered from the interleaved table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TauriAsset {
    /// Asset key exactly as embedded, such as `/assets/main.js`.
    pub key: String,
    /// File offset of the compressed brotli blob.
    pub blob_offset: u64,
    /// Compressed length in bytes.
    pub compressed_len: usize,
    /// Decompressed length in bytes.
    pub decompressed_len: usize,
    /// First [`PREVIEW_BYTES`] decompressed bytes rendered as text; empty for
    /// binary assets such as icons.
    pub preview: String,
    /// Path the decoded asset was written to, absent without an output directory.
    pub file: Option<String>,
}

/// Bounded result of one [`analyze`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TauriAssetsReport {
    /// Assets recovered from the longest interleaved run, cut at the limit.
    pub assets: Vec<TauriAsset>,
    /// Number of complete `(key, blob)` pairs the run held before the limit.
    pub total: usize,
    /// Whether the limit cut the asset list short.
    pub truncated: bool,
    /// Output directory the assets were written to, when requested.
    pub output_dir: Option<String>,
}

/// Extracts the embedded assets of a Tauri v2 PE image.
pub fn analyze(image: &[u8], options: &TauriAssetsOptions) -> Result<TauriAssetsReport> {
    let parsed = pe::parse_pe(image).map_err(|error| ContainerError::parse(format!("PE: {error}")))?;
    let image_base = parsed.optional.image_base;
    let sections = &parsed.sections;
    let resolve = |va: u64| -> Option<usize> {
        let rva = va.checked_sub(image_base)?;
        if rva > u64::from(u32::MAX) {
            return None;
        }
        let offset = pe::rva_to_offset(sections, rva as u32).ok()?;
        usize::try_from(offset).ok()
    };
    scan_assets(image, &resolve, options)
}

/// Finds the longest interleaved `(key, blob)` run and decodes its assets.
///
/// `resolve_va` maps a recorded virtual address onto a file offset; tests use
/// a 1:1 identity layout while the PE wrapper resolves through the section
/// table. The scan never filters by printable ratio, so binary assets such as
/// `icon.png` survive alongside text ones.
pub fn scan_assets(
    data: &[u8],
    resolve_va: &dyn Fn(u64) -> Option<usize>,
    options: &TauriAssetsOptions,
) -> Result<TauriAssetsReport> {
    let limit = options.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let output_dir = options.output_dir.as_deref().map(Path::new);

    let mut best: Option<(usize, usize)> = None;
    let mut cursor = 0_usize;
    while cursor + ENTRY_STRIDE <= data.len() {
        if key_record(data, resolve_va, cursor).is_some() && blob_record(data, resolve_va, cursor + RECORD_STRIDE).is_some() {
            let mut count = 1_usize;
            let mut next = cursor + ENTRY_STRIDE;
            while next + ENTRY_STRIDE <= data.len()
                && key_record(data, resolve_va, next).is_some()
                && blob_record(data, resolve_va, next + RECORD_STRIDE).is_some()
            {
                count += 1;
                next += ENTRY_STRIDE;
            }
            if count >= 2 && best.is_none_or(|(_, held)| count > held) {
                best = Some((cursor, count));
            }
            cursor = next;
            continue;
        }
        cursor += 8;
    }

    let Some((table_offset, count)) = best
    else {
        return Ok(TauriAssetsReport {
            assets: Vec::new(),
            total: 0,
            truncated: false,
            output_dir: options.output_dir.clone(),
        });
    };

    let keep = count.min(limit);
    let mut assets = Vec::with_capacity(keep);
    for index in 0..keep {
        let entry = table_offset + index * ENTRY_STRIDE;
        let (key_offset, key_len) = key_record(data, resolve_va, entry).expect("validated during the run scan");
        let (blob_offset, blob_len) =
            blob_record(data, resolve_va, entry + RECORD_STRIDE).expect("validated during the run scan");
        let key = String::from_utf8_lossy(&data[key_offset..key_offset + key_len]).into_owned();
        let decoded = brotli_decode(data, blob_offset, blob_len).expect("validated during the run scan");
        let file = match output_dir {
            Some(dir) => Some(write_asset(dir, &key, &decoded)?),
            None => None,
        };
        assets.push(TauriAsset {
            key,
            blob_offset: blob_offset as u64,
            compressed_len: blob_len,
            decompressed_len: decoded.len(),
            preview: preview(&decoded),
            file,
        });
    }

    Ok(TauriAssetsReport { assets, total: count, truncated: keep < count, output_dir: options.output_dir.clone() })
}

/// Resolves one key record at `at`: its target is a printable `/…` path.
fn key_record(data: &[u8], resolve_va: &dyn Fn(u64) -> Option<usize>, at: usize) -> Option<(usize, usize)> {
    let pointer = read_u64(data, at)?;
    let length = read_u64(data, at + 8)?;
    if !(MIN_KEY_LEN..=MAX_KEY_LEN).contains(&length) {
        return None;
    }
    let offset = resolve_va(pointer)?;
    let end = offset.checked_add(length as usize)?;
    if end > data.len() || data[offset] != b'/' {
        return None;
    }
    let text = &data[offset..end];
    let printable = text.iter().all(|byte| (0x20..=0x7e).contains(byte));
    if printable && text.contains(&b'.') { Some((offset, length as usize)) } else { None }
}

/// Resolves one blob record at `at`: its target brotli-decodes completely.
fn blob_record(data: &[u8], resolve_va: &dyn Fn(u64) -> Option<usize>, at: usize) -> Option<(usize, usize)> {
    let pointer = read_u64(data, at)?;
    let length = read_u64(data, at + 8)?;
    if length == 0 || length > data.len() as u64 {
        return None;
    }
    let offset = resolve_va(pointer)?;
    let end = offset.checked_add(length as usize)?;
    if end > data.len() {
        return None;
    }
    // Only the length is needed here; kept assets are re-decoded during
    // collection so a huge table never has to be held in memory at once.
    brotli_decode(data, offset, length as usize)?;
    Some((offset, length as usize))
}

/// Decodes one standalone brotli stream bounded by [`MAX_DECODED_ASSET`].
///
/// The declared length must hold exactly one stream: a decode that stops early
/// or leaves trailing bytes is rejected, matching the record contract above.
fn brotli_decode(data: &[u8], offset: usize, length: usize) -> Option<Vec<u8>> {
    let end = offset.checked_add(length)?;
    let input = data.get(offset..end)?;
    let limits = CodecLimits::new().with_max_output_bytes(MAX_DECODED_ASSET as u64);
    let decoded = Brotli::decode_bounded(input, limits);
    if !decoded.is_complete() || decoded.trailing_input != 0 {
        return None;
    }
    Some(decoded.output)
}

/// Writes one decoded asset under `dir`, mirroring its key path.
fn write_asset(dir: &Path, key: &str, bytes: &[u8]) -> Result<String> {
    let relative = key.strip_prefix('/').unwrap_or(key);
    if relative.is_empty() || relative.contains("..") || relative.contains('\\') {
        return Err(ContainerError::parse(format!("unsafe asset key: {key}")));
    }
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(path.display().to_string())
}

/// Renders the first [`PREVIEW_BYTES`] decompressed bytes when they are mostly
/// printable; binary payloads keep an empty preview.
fn preview(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(PREVIEW_BYTES)];
    if head.is_empty() || printable_ratio(head) < MIN_PREVIEW_RATIO {
        return String::new();
    }
    head.iter()
        .map(
            |&byte| {
                if byte.is_ascii_graphic() || matches!(byte, b' ' | b'\t' | b'\n' | b'\r') { char::from(byte) } else { '.' }
            },
        )
        .collect()
}

/// Share of bytes that are printable ASCII or a common control byte.
fn printable_ratio(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let printable =
        bytes.iter().filter(|&&byte| (0x20..=0x7e).contains(&byte) || matches!(byte, b'\t' | b'\n' | b'\r')).count();
    printable as f64 / bytes.len() as f64
}

/// Reads one little-endian `u64`, returning `None` past the end of the buffer.
fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let window = data.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(window.try_into().expect("eight-byte window")))
}

#[cfg(test)]
mod tests {
    use super::brotli_decode;

    /// `Hello, Brotli! This is a short ASCII fixture.`, produced once with
    /// Node.js 24 `zlib.brotliCompressSync` at quality 5 and window bits 22.
    const SHORT_Q5: &str = "1b2c0000443797eaec62e86ab9f921ff201c0ee2e15d8822bf2824bd0f74c45a86d5ed93c480271e747a0476fe7eb504";

    const PLAINTEXT: &[u8] = b"Hello, Brotli! This is a short ASCII fixture.";

    fn vector() -> Vec<u8> {
        (0..SHORT_Q5.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&SHORT_Q5[index..index + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn decodes_one_complete_stream() {
        let input = vector();
        assert_eq!(brotli_decode(&input, 0, input.len()).as_deref(), Some(PLAINTEXT));
    }

    #[test]
    fn trailing_bytes_after_the_stream_are_rejected() {
        let mut input = vector();
        input.extend_from_slice(&[0xff; 32]);
        assert!(brotli_decode(&input, 0, input.len()).is_none());
    }

    #[test]
    fn truncated_stream_is_rejected() {
        let input = vector();
        let cut = &input[..input.len() / 2];
        assert!(brotli_decode(cut, 0, cut.len()).is_none());
    }
}
