//! PNG decoder (RFC 2083 / ISO/IEC 15948), written from the specification.
//!
//! Supported: bit depth 8 and 16 for greyscale, RGB, greyscale-alpha and
//! RGBA, bit depth 1/2/4/8 for indexed colour, the five per-row filters, and
//! `tRNS` transparency for every colour type. `gAMA` is parsed and validated
//! but not applied — colour management is not a storage concern. Adam7
//! interlacing is refused explicitly.

use data_compression::{CodecLimits, Zlib};

use super::{DecodeError, DecodeLimits, DecodedImage, Orientation};
use crate::Bitmap;

/// The eight-byte PNG signature every conforming file starts with.
pub(crate) const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Decodes a PNG file into an RGBA8 [`Bitmap`].
///
/// The pixel budget is checked against `IHDR` before any buffer is allocated,
/// and the inflated `IDAT` payload is capped at the exact raw size the header
/// declares. PNG carries no orientation, so the result is always
/// [`Orientation::TopLeft`].
pub fn decode_png(bytes: &[u8], limits: DecodeLimits) -> Result<DecodedImage, DecodeError> {
    if !bytes.starts_with(SIGNATURE) {
        return Err(DecodeError::WrongSignature);
    }

    let mut header: Option<Header> = None;
    let mut palette: Vec<u8> = Vec::new();
    let mut transparency: Vec<u8> = Vec::new();
    let mut gamma: Option<u32> = None;
    let mut compressed: Vec<u8> = Vec::new();
    let mut seen_idat = false;
    let mut seen_iend = false;

    let mut cursor = SIGNATURE.len();
    while cursor < bytes.len() && !seen_iend {
        let (kind, payload, next) = read_chunk(bytes, cursor)?;
        cursor = next;

        match kind.as_slice() {
            b"IHDR" => {
                if header.is_some() {
                    return Err(DecodeError::Malformed("duplicate IHDR chunk"));
                }
                if seen_idat {
                    return Err(DecodeError::Malformed("IHDR after IDAT"));
                }
                if payload.len() != 13 {
                    return Err(DecodeError::Malformed("IHDR must be 13 bytes"));
                }
                header = Some(Header::read(payload)?);
            }
            b"PLTE" => {
                if payload.is_empty() || payload.len() % 3 != 0 || payload.len() > 768 {
                    return Err(DecodeError::Malformed("PLTE length must be a non-zero multiple of three up to 768"));
                }
                palette.extend_from_slice(payload);
            }
            b"tRNS" => transparency.extend_from_slice(payload),
            b"gAMA" => {
                if payload.len() != 4 {
                    return Err(DecodeError::Malformed("gAMA must be 4 bytes"));
                }
                gamma = Some(be32(payload, 0));
            }
            b"IDAT" => {
                if header.is_none() {
                    return Err(DecodeError::Malformed("IDAT before IHDR"));
                }
                seen_idat = true;
                compressed.extend_from_slice(payload);
            }
            b"IEND" => seen_iend = true,
            _ => {}
        }
    }

    let header = header.ok_or(DecodeError::Malformed("missing IHDR chunk"))?;
    if !seen_idat {
        return Err(DecodeError::Malformed("missing IDAT chunk"));
    }
    if !seen_iend {
        return Err(DecodeError::Malformed("missing IEND chunk"));
    }
    let _ = gamma; // parsed and validated, deliberately not applied

    limits.check_size(header.width, header.height)?;
    if header.color_type == 3 && palette.is_empty() {
        return Err(DecodeError::Malformed("indexed PNG without PLTE"));
    }

    let raw_len = (header.height as usize) * (1 + header.row_bytes());
    let codec_limits = CodecLimits::new().with_max_output_bytes(raw_len as u64);
    let inflated = Zlib::decode(&compressed, codec_limits)
        .map_err(|error| {
            if error.is_resource_limit() {
                DecodeError::Malformed("IDAT inflated beyond the size IHDR declares")
            }
            else {
                DecodeError::Compressed(error.to_string())
            }
        })?
        .value;
    if inflated.len() != raw_len {
        return Err(DecodeError::Malformed("IDAT payload shorter than the size IHDR declares"));
    }

    let rows = unfilter(&inflated, header.height as usize, header.row_bytes(), header.filter_bpp())?;
    let rgba = to_rgba8(&rows, &header, &palette, &transparency)?;
    Ok(DecodedImage { bitmap: Bitmap::new(header.width, header.height, rgba), orientation: Orientation::TopLeft })
}

/// Reads one `length type data crc` chunk and verifies the CRC.
fn read_chunk(bytes: &[u8], start: usize) -> Result<([u8; 4], &[u8], usize), DecodeError> {
    if bytes.len() - start < 8 {
        return Err(DecodeError::Truncated { offset: bytes.len() });
    }
    let length = be32(bytes, start) as usize;
    let kind = [bytes[start + 4], bytes[start + 5], bytes[start + 6], bytes[start + 7]];
    let body_end = match start.checked_add(8).and_then(|data_start| data_start.checked_add(length)) {
        Some(end) => end,
        None => return Err(DecodeError::Malformed("chunk length overflows the file size")),
    };
    if bytes.len() < body_end + 4 {
        return Err(DecodeError::Truncated { offset: bytes.len() });
    }
    let stored = be32(bytes, body_end);
    let actual = data_checksum::crc32(&bytes[start + 4..body_end]);
    if stored != actual {
        return Err(DecodeError::ChunkCrc { kind, expected: stored, actual });
    }
    Ok((kind, &bytes[start + 8..body_end], body_end + 4))
}

/// The parsed `IHDR` fields.
struct Header {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
}

impl Header {
    fn read(data: &[u8]) -> Result<Self, DecodeError> {
        let header = Self { width: be32(data, 0), height: be32(data, 4), bit_depth: data[8], color_type: data[9] };
        if header.width == 0 || header.height == 0 {
            return Err(DecodeError::Malformed("IHDR declares a zero dimension"));
        }
        if data[10] != 0 {
            return Err(DecodeError::Unsupported("non-deflate PNG compression"));
        }
        if data[11] != 0 {
            return Err(DecodeError::Unsupported("non-adaptive PNG filter method"));
        }
        if data[12] != 0 {
            return Err(DecodeError::Unsupported("interlaced PNG (Adam7)"));
        }
        let depth_ok = match header.color_type {
            0 | 2 | 4 | 6 => matches!(header.bit_depth, 8 | 16),
            3 => matches!(header.bit_depth, 1 | 2 | 4 | 8),
            _ => return Err(DecodeError::Unsupported("reserved PNG colour type")),
        };
        if !depth_ok {
            return Err(DecodeError::Unsupported("PNG bit depth invalid for its colour type"));
        }
        Ok(header)
    }

    /// Samples per channel for this colour type.
    fn channels(&self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }

    /// Unfiltered bytes in one row, excluding the leading filter byte.
    fn row_bytes(&self) -> usize {
        if self.color_type == 3 {
            (self.width as usize * self.bit_depth as usize + 7) / 8
        }
        else {
            self.width as usize * self.channels() * (self.bit_depth as usize / 8)
        }
    }

    /// Filter bytes-per-pixel step: at least one byte, per the spec's
    /// definition of `bpp` for the row filters.
    fn filter_bpp(&self) -> usize {
        (self.channels() * self.bit_depth as usize / 8).max(1)
    }
}

/// Reverses the five per-row filters (RFC 2083 §6).
fn unfilter(data: &[u8], height: usize, row_bytes: usize, bpp: usize) -> Result<Vec<u8>, DecodeError> {
    let mut out = vec![0u8; height * row_bytes];
    for y in 0..height {
        let filter = data[y * (row_bytes + 1)];
        let line = &data[y * (row_bytes + 1) + 1..(y + 1) * (row_bytes + 1)];
        let (decoded, rest) = out.split_at_mut(y * row_bytes);
        let dst = &mut rest[..row_bytes];
        let above: Option<&[u8]> = if y == 0 { None } else { Some(&decoded[(y - 1) * row_bytes..y * row_bytes]) };
        match filter {
            0 => dst.copy_from_slice(line),
            1 => {
                for (i, byte) in line.iter().enumerate() {
                    let left = if i >= bpp { u32::from(dst[i - bpp]) } else { 0 };
                    dst[i] = (u32::from(*byte) + left) as u8;
                }
            }
            2 => {
                for (i, byte) in line.iter().enumerate() {
                    let up = above.map_or(0, |row| u32::from(row[i]));
                    dst[i] = (u32::from(*byte) + up) as u8;
                }
            }
            3 => {
                for (i, byte) in line.iter().enumerate() {
                    let left = if i >= bpp { u32::from(dst[i - bpp]) } else { 0 };
                    let up = above.map_or(0, |row| u32::from(row[i]));
                    dst[i] = (u32::from(*byte) + (left + up) / 2) as u8;
                }
            }
            4 => {
                for (i, byte) in line.iter().enumerate() {
                    let left = if i >= bpp { u32::from(dst[i - bpp]) } else { 0 };
                    let up = above.map_or(0, |row| u32::from(row[i]));
                    let up_left = if i >= bpp { above.map_or(0, |row| u32::from(row[i - bpp])) } else { 0 };
                    dst[i] = (u32::from(*byte) + paeth(left, up, up_left)) as u8;
                }
            }
            _ => return Err(DecodeError::Malformed("unknown PNG row filter type")),
        }
    }
    Ok(out)
}

fn paeth(a: u32, b: u32, c: u32) -> u32 {
    let p = a + b - c;
    let (pa, pb, pc) = (p.abs_diff(a), p.abs_diff(b), p.abs_diff(c));
    if pa <= pb && pa <= pc {
        a
    }
    else if pb <= pc {
        b
    }
    else {
        c
    }
}

/// Expands unfiltered scanlines into RGBA8 bytes.
fn to_rgba8(rows: &[u8], header: &Header, palette: &[u8], transparency: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let width = header.width as usize;
    let height = header.height as usize;
    let row_bytes = header.row_bytes();
    let mut rgba = vec![0u8; width * height * 4];

    let sample_bits = header.bit_depth as usize;
    match header.color_type {
        0 => {
            for y in 0..height {
                let row = &rows[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width {
                    let sample = raw_sample(row, x * sample_bits, header.bit_depth);
                    let alpha = if transparency.len() >= 2 && sample == be16(transparency, 0) { 0 } else { 255 };
                    let value = fold(sample, header.bit_depth);
                    let d = (y * width + x) * 4;
                    rgba[d..d + 4].copy_from_slice(&[value, value, value, alpha]);
                }
            }
        }
        4 => {
            for y in 0..height {
                let row = &rows[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width {
                    let gray = fold(raw_sample(row, x * 2 * sample_bits, header.bit_depth), header.bit_depth);
                    let alpha = fold(raw_sample(row, (x * 2 + 1) * sample_bits, header.bit_depth), header.bit_depth);
                    let d = (y * width + x) * 4;
                    rgba[d..d + 4].copy_from_slice(&[gray, gray, gray, alpha]);
                }
            }
        }
        2 => {
            for y in 0..height {
                let row = &rows[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width {
                    let r = raw_sample(row, x * 3 * sample_bits, header.bit_depth);
                    let g = raw_sample(row, (x * 3 + 1) * sample_bits, header.bit_depth);
                    let b = raw_sample(row, (x * 3 + 2) * sample_bits, header.bit_depth);
                    let transparent =
                        transparency.len() >= 6 && r == be16(transparency, 0) && g == be16(transparency, 2) && b == be16(transparency, 4);
                    let d = (y * width + x) * 4;
                    rgba[d..d + 4].copy_from_slice(&[
                        fold(r, header.bit_depth),
                        fold(g, header.bit_depth),
                        fold(b, header.bit_depth),
                        if transparent { 0 } else { 255 },
                    ]);
                }
            }
        }
        6 => {
            for y in 0..height {
                let row = &rows[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width {
                    let base = x * 4 * sample_bits;
                    let d = (y * width + x) * 4;
                    rgba[d] = fold(raw_sample(row, base, header.bit_depth), header.bit_depth);
                    rgba[d + 1] = fold(raw_sample(row, base + sample_bits, header.bit_depth), header.bit_depth);
                    rgba[d + 2] = fold(raw_sample(row, base + 2 * sample_bits, header.bit_depth), header.bit_depth);
                    rgba[d + 3] = fold(raw_sample(row, base + 3 * sample_bits, header.bit_depth), header.bit_depth);
                }
            }
        }
        3 => {
            let colors = palette.len() / 3;
            for y in 0..height {
                let row = &rows[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width {
                    let index = sub_byte(row, x, header.bit_depth) as usize;
                    if index >= colors {
                        return Err(DecodeError::Malformed("palette index out of range"));
                    }
                    let p = index * 3;
                    let alpha = transparency.get(index).copied().unwrap_or(255);
                    let d = (y * width + x) * 4;
                    rgba[d..d + 4].copy_from_slice(&[palette[p], palette[p + 1], palette[p + 2], alpha]);
                }
            }
        }
        _ => return Err(DecodeError::Unsupported("reserved PNG colour type")),
    }
    Ok(rgba)
}

/// Reads one big-endian sample of `bit_depth` bits at `bit_offset`.
fn raw_sample(row: &[u8], bit_offset: usize, bit_depth: u8) -> u16 {
    if bit_depth == 8 { u16::from(row[bit_offset / 8]) } else { be16(row, bit_offset / 8) }
}

/// Folds a stored sample down to 8 bits: 8-bit samples pass through, 16-bit
/// samples keep the high byte.
fn fold(sample: u16, bit_depth: u8) -> u8 {
    if bit_depth == 16 { (sample >> 8) as u8 } else { sample as u8 }
}

/// Reads one sub-byte palette index from a row (most significant bit first).
fn sub_byte(row: &[u8], x: usize, bit_depth: u8) -> u8 {
    match bit_depth {
        8 => row[x],
        4 => {
            let byte = row[x / 2];
            if x % 2 == 0 { byte >> 4 } else { byte & 0x0F }
        }
        2 => {
            let byte = row[x / 4];
            (byte >> (6 - 2 * (x % 4))) & 0x03
        }
        _ => {
            let byte = row[x / 8];
            (byte >> (7 - x % 8)) & 0x01
        }
    }
}

const fn be32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
}

const fn be16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}
