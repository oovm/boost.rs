//! Minimal PNG writer (8-bit samples, no interlace).
//!
//! Used to export still frames and to generate media fixtures without pulling
//! in an imaging library. The deflate stream is stored, not compressed: these
//! files are small, and correctness matters more than ratio here.

use data_checksum::crc32;
use data_compression::{DeflateLevel, Zlib};
use media_types::Error;

/// Sample layout of an 8-bit PNG image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColor {
    /// One gray sample per pixel.
    Gray,
    /// Red, green and blue samples per pixel.
    Rgb,
    /// Gray plus alpha samples per pixel.
    GrayAlpha,
    /// Red, green, blue and alpha samples per pixel.
    Rgba,
}

impl PngColor {
    /// PNG `IHDR` color type code.
    pub fn color_type(self) -> u8 {
        match self {
            Self::Gray => 0,
            Self::Rgb => 2,
            Self::GrayAlpha => 4,
            Self::Rgba => 6,
        }
    }

    /// Number of samples stored per pixel.
    pub fn channels(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
            Self::GrayAlpha => 2,
            Self::Rgba => 4,
        }
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(kind);
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// Encodes RGBA8 pixels as PNG bytes.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Error> {
    encode_png_with_color(width, height, PngColor::Rgba, rgba)
}

/// Encodes 8-bit pixels in the given layout as PNG bytes.
///
/// `pixels` must hold `width * height * color.channels()` samples in row-major
/// order. Extra trailing bytes are ignored, a short buffer is an error.
pub fn encode_png_with_color(width: u32, height: u32, color: PngColor, pixels: &[u8]) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 {
        return Err(Error::invalid("png: zero dimension"));
    }
    let pixel_bytes = (width as usize) * (height as usize) * color.channels();
    if pixels.len() < pixel_bytes {
        return Err(Error::invalid(format!("png: expected {pixel_bytes} bytes, got {}", pixels.len())));
    }

    // Raw scanlines, each prefixed with filter type 0.
    let mut raw = Vec::with_capacity(pixel_bytes + height as usize);
    for y in 0..height as usize {
        raw.push(0u8);
        let start = y * (width as usize) * color.channels();
        raw.extend_from_slice(&pixels[start..start + (width as usize) * color.channels()]);
    }

    // The zlib envelope, its Adler-32 trailer, and the stored Deflate blocks
    // all belong to `data-compression`; the stored level keeps the stream
    // byte-identical to the previous hand-written writer.
    let zlib = Zlib::encode(&raw, DeflateLevel::Store);

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color.color_type(), 0, 0, 0]); // 8-bit samples, deflate, adaptive filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// Encodes RGBA8 pixels as a PNG file and writes it to `path`.
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), Error> {
    let bytes = encode_png(width, height, rgba)?;
    std::fs::write(path, bytes)?;
    Ok(())
}
