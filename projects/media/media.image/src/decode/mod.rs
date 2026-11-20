//! Portable pure-Rust decoders for PNG and baseline JPEG.
//!
//! These decoders read the format specifications directly (RFC 2083 / ITU-T
//! T.81) and never call a host codec: the same bytes decode the same way on
//! every platform `media.image` runs on. Anything outside the supported
//! variant set (interlaced PNG, progressive JPEG, arithmetic coding) fails
//! with an explicit [`DecodeError::Unsupported`] instead of guessing, and
//! allocation is bounded by [`DecodeLimits::max_pixels`] before any pixel
//! buffer is created.
//!
//! EXIF orientation is reported as a field on [`DecodedImage`]; the decoder
//! never rotates pixels itself. Applying it is the consumer's decision.

mod exif;
mod jpeg;
mod png;

use std::fmt;

use crate::Bitmap;

pub use jpeg::decode_jpeg;
pub use png::decode_png;

/// EXIF orientation of the stored pixels, values 1..=8.
///
/// The transform describes how to map the stored grid to the intended
/// display: `TopLeft` stores the image as it should be shown, the other
/// variants are rotations and mirrors applied in the direction the name
/// reads (e.g. `RightTop` is the stored grid rotated 90° clockwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Orientation {
    /// Row 0 is the top, column 0 is the left: store as-is.
    #[default]
    TopLeft = 1,
    /// Mirror along the vertical axis.
    TopRight = 2,
    /// Rotate 180°.
    BottomRight = 3,
    /// Mirror along the horizontal axis.
    BottomLeft = 4,
    /// Mirror along the top-left diagonal (transpose).
    LeftTop = 5,
    /// Rotate 90° clockwise.
    RightTop = 6,
    /// Mirror along the top-right diagonal (anti-transpose).
    LeftBottom = 7,
    /// Rotate 90° counter-clockwise.
    RightBottom = 8,
}

impl Orientation {
    /// Maps a raw EXIF `Orientation` tag value onto the enum.
    #[must_use]
    pub const fn from_exif(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::TopLeft),
            2 => Some(Self::TopRight),
            3 => Some(Self::BottomRight),
            4 => Some(Self::BottomLeft),
            5 => Some(Self::LeftTop),
            6 => Some(Self::RightTop),
            7 => Some(Self::LeftBottom),
            8 => Some(Self::RightBottom),
            _ => None,
        }
    }

    /// The raw EXIF tag value for this orientation.
    #[must_use]
    pub const fn to_exif(self) -> u8 {
        self as u8
    }
}

/// Allocation budget one decode call may not exceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum decoded pixel count (width × height) checked against the
    /// header before any pixel buffer is allocated.
    pub max_pixels: usize,
}

impl DecodeLimits {
    /// No cap: the container's declared size is trusted.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self { max_pixels: usize::MAX }
    }

    /// A pixel budget: anything whose header declares more pixels fails
    /// before allocation.
    #[must_use]
    pub const fn pixel_budget(max_pixels: usize) -> Self {
        Self { max_pixels }
    }

    /// Checks a header-declared size against the budget.
    pub(crate) fn check_size(self, width: u32, height: u32) -> Result<(), DecodeError> {
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        if pixels > self.max_pixels as u64 {
            return Err(DecodeError::PixelBudget { width, height, max_pixels: self.max_pixels });
        }
        Ok(())
    }
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::unbounded()
    }
}

/// A decoded still image and the orientation its container declared.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    /// Pixels as stored by the container, RGBA8.
    pub bitmap: Bitmap,
    /// EXIF orientation of `bitmap`; `TopLeft` when no orientation was found.
    pub orientation: Orientation,
}

/// Why a byte stream was not decodable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes do not start with the signature of the expected format.
    WrongSignature,
    /// Input ended in the middle of a structure at `offset`.
    Truncated {
        /// Byte offset where the input ran out.
        offset: usize,
    },
    /// The CRC of chunk `kind` did not match its payload.
    ChunkCrc {
        /// Four-letter chunk type.
        kind: [u8; 4],
        /// Checksum stored in the file.
        expected: u32,
        /// Checksum computed over the payload.
        actual: u32,
    },
    /// The stream uses a variant this decoder deliberately does not support.
    Unsupported(&'static str),
    /// The structure is malformed: bad marker, missing or duplicated chunk.
    Malformed(&'static str),
    /// The header declares more pixels than the budget allows.
    PixelBudget {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Budget that was exceeded.
        max_pixels: usize,
    },
    /// The compressed payload failed to inflate.
    Compressed(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSignature => f.write_str("bytes do not carry a PNG or JPEG signature"),
            Self::Truncated { offset } => write!(f, "input ended at offset {offset}"),
            Self::ChunkCrc { kind, expected, actual } => {
                let kind = core::str::from_utf8(kind).unwrap_or("????");
                write!(f, "chunk {kind} CRC mismatch: expected {expected:#010x}, computed {actual:#010x}")
            }
            Self::Unsupported(variant) => write!(f, "unsupported variant: {variant}"),
            Self::Malformed(detail) => write!(f, "malformed stream: {detail}"),
            Self::PixelBudget { width, height, max_pixels } => {
                write!(f, "declared {width}x{height} exceeds the {max_pixels} pixel budget")
            }
            Self::Compressed(detail) => write!(f, "compressed payload failed: {detail}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decodes `bytes` by sniffing the container signature.
///
/// PNG goes to [`decode_png`], JPEG to [`decode_jpeg`]; anything else fails
/// with [`DecodeError::WrongSignature`] without touching the payload.
pub fn decode(bytes: &[u8], limits: DecodeLimits) -> Result<DecodedImage, DecodeError> {
    if bytes.starts_with(png::SIGNATURE) {
        decode_png(bytes, limits)
    }
    else if bytes.starts_with(jpeg::SIGNATURE) {
        decode_jpeg(bytes, limits)
    }
    else {
        Err(DecodeError::WrongSignature)
    }
}

#[cfg(test)]
mod tests {
    use data_compression::{DeflateLevel, Zlib};

    use super::*;

    /// Wraps a chunk payload in the PNG `length type data crc` framing.
    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(payload.len() + 12);
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(kind);
        framed.extend_from_slice(payload);
        let mut crc_input = Vec::with_capacity(payload.len() + 4);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(payload);
        framed.extend_from_slice(&data_checksum::crc32(&crc_input).to_be_bytes());
        framed
    }

    /// Builds a PNG whose scanlines all use filter type 0.
    fn png(width: u32, height: u32, bit_depth: u8, color_type: u8, interlace: u8, pixels: &[u8]) -> Vec<u8> {
        let channels = match color_type {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        };
        let row = width as usize * channels * (bit_depth as usize / 8).max(1);
        assert_eq!(pixels.len(), row * height as usize);

        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[bit_depth, color_type, 0, 0, interlace]);

        let mut raw = Vec::with_capacity((row + 1) * height as usize);
        for y in 0..height as usize {
            raw.push(0);
            raw.extend_from_slice(&pixels[y * row..(y + 1) * row]);
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(png::SIGNATURE);
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"IDAT", &Zlib::encode(&raw, DeflateLevel::Store)));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        bytes
    }

    /// An 8x8 baseline JPEG of a solid colour, produced by a mainstream encoder.
    const JPEG_8X8: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x01, 0x00, 0x60, 0x00, 0x60, 0x00, 0x00, 0xff, 0xdb,
        0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x03, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x03, 0x03, 0x04, 0x05, 0x08, 0x05, 0x05, 0x04,
        0x04, 0x05, 0x0a, 0x07, 0x07, 0x06, 0x08, 0x0c, 0x0a, 0x0c, 0x0c, 0x0b, 0x0a, 0x0b, 0x0b, 0x0d, 0x0e, 0x12, 0x10, 0x0d, 0x0e, 0x11,
        0x0e, 0x0b, 0x0b, 0x10, 0x16, 0x10, 0x11, 0x13, 0x14, 0x15, 0x15, 0x15, 0x0c, 0x0f, 0x17, 0x18, 0x16, 0x14, 0x18, 0x12, 0x14, 0x15,
        0x14, 0xff, 0xdb, 0x00, 0x43, 0x01, 0x03, 0x04, 0x04, 0x05, 0x04, 0x05, 0x09, 0x05, 0x05, 0x09, 0x14, 0x0d, 0x0b, 0x0d, 0x14, 0x14,
        0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14,
        0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14,
        0x14, 0x14, 0x14, 0x14, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x08, 0x00, 0x08, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11,
        0x01, 0xff, 0xc4, 0x00, 0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03,
        0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41,
        0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24,
        0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38,
        0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
        0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
        0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1,
        0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xc4, 0x00,
        0x1f, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03,
        0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07,
        0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a,
        0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43,
        0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95,
        0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9,
        0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4,
        0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00,
        0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00, 0xf1, 0x2a, 0x28, 0xa2, 0xbf, 0x2a, 0x3f, 0xbf, 0x4f, 0xff, 0xd9,
    ];

    #[test]
    fn png_rgba_pixels_round_trip() {
        let pixels = [255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 128, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let image = decode(&png(3, 2, 8, 6, 0, &pixels), DecodeLimits::unbounded()).expect("decode");
        assert_eq!(image.bitmap.width, 3);
        assert_eq!(image.bitmap.height, 2);
        assert_eq!(image.bitmap.rgba, pixels);
        assert_eq!(image.orientation, Orientation::TopLeft, "PNG carries no orientation");
    }

    #[test]
    fn png_grey_and_rgb_expand_to_opaque_rgba() {
        let grey = decode(&png(2, 1, 8, 0, 0, &[0, 255]), DecodeLimits::unbounded()).expect("grey decode");
        assert_eq!(grey.bitmap.rgba, [0, 0, 0, 255, 255, 255, 255, 255]);
        let rgb = decode(&png(2, 1, 8, 2, 0, &[1, 2, 3, 4, 5, 6]), DecodeLimits::unbounded()).expect("rgb decode");
        assert_eq!(rgb.bitmap.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn png_declared_size_is_checked_before_allocating() {
        let bytes = png(3, 2, 8, 6, 0, &[0; 24]);
        let error = decode(&bytes, DecodeLimits::pixel_budget(5)).expect_err("budget");
        assert_eq!(error, DecodeError::PixelBudget { width: 3, height: 2, max_pixels: 5 });
    }

    #[test]
    fn png_chunk_crc_mismatch_is_reported() {
        let mut bytes = png(1, 1, 8, 6, 0, &[1, 2, 3, 4]);
        // First byte of the IHDR payload: the width. Its CRC no longer matches.
        bytes[16] ^= 1;
        let error = decode(&bytes, DecodeLimits::unbounded()).expect_err("crc");
        assert!(matches!(error, DecodeError::ChunkCrc { kind, .. } if &kind == b"IHDR"), "unexpected error: {error:?}");
    }

    #[test]
    fn png_interlaced_streams_are_refused() {
        let bytes = png(1, 1, 8, 6, 1, &[1, 2, 3, 4]);
        assert_eq!(decode(&bytes, DecodeLimits::unbounded()).expect_err("interlaced"), DecodeError::Unsupported("interlaced PNG (Adam7)"));
    }

    #[test]
    fn jpeg_baseline_decodes_to_rgba_without_orientation() {
        let image = decode(JPEG_8X8, DecodeLimits::unbounded()).expect("jpeg decode");
        assert_eq!((image.bitmap.width, image.bitmap.height), (8, 8));
        assert_eq!(image.orientation, Orientation::TopLeft);
        let pixel = &image.bitmap.rgba[..4];
        assert!(pixel[0] > 180 && pixel[1] < 80 && pixel[2] < 80, "expected a red pixel, got {pixel:?}");
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn jpeg_respects_the_pixel_budget() {
        let error = decode(JPEG_8X8, DecodeLimits::pixel_budget(10)).expect_err("budget");
        assert_eq!(error, DecodeError::PixelBudget { width: 8, height: 8, max_pixels: 10 });
    }

    #[test]
    fn unknown_signatures_are_rejected_without_parsing() {
        assert_eq!(decode(b"not an image", DecodeLimits::unbounded()).expect_err("signature"), DecodeError::WrongSignature);
    }
}
