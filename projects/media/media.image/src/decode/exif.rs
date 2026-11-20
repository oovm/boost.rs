//! Minimal EXIF reading: the `Orientation` tag and nothing else.

use super::Orientation;

/// The EXIF `Orientation` tag id.
const TAG_ORIENTATION: u16 = 0x0112;

/// Extracts the orientation from an APP1 payload that starts with the
/// `Exif\0\0` prefix, following the TIFF header and IFD0 entries.
///
/// Anything unrecognised — no prefix, bad TIFF magic, missing tag, wrong
/// type — yields `None` instead of an error: orientation is metadata, and a
/// broken EXIF block must not make an otherwise decodable image fail.
pub(crate) fn orientation_from_app1(payload: &[u8]) -> Option<Orientation> {
    let tiff = payload.strip_prefix(b"Exif\x00\x00")?;
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let magic = u16_value(tiff, 2, little);
    if magic != 42 {
        return None;
    }
    let mut ifd = usize::try_from(u32_value(tiff, 4, little)).ok()?;
    for _ in 0..4 {
        // One level of sub-IFD walking is enough: IFD0 may point at Exif
        // or GPS sub-IFDs, but orientation lives in IFD0 itself.
        if ifd + 2 > tiff.len() {
            return None;
        }
        let count = usize::from(u16_value(tiff, ifd, little));
        if ifd + 2 + count.checked_mul(12)? > tiff.len() {
            return None;
        }
        for entry in 0..count {
            let base = ifd + 2 + entry * 12;
            let tag = u16_value(tiff, base, little);
            if tag != TAG_ORIENTATION {
                continue;
            }
            let kind = u16_value(tiff, base + 2, little);
            if kind != 3 {
                return None;
            }
            let value = u16_value(tiff, base + 8, little);
            return Orientation::from_exif(u8::try_from(value).ok()?);
        }
        if ifd + 2 + count * 12 + 4 > tiff.len() {
            return None;
        }
        ifd = usize::try_from(u32_value(tiff, ifd + 2 + count * 12, little)).ok()?;
    }
    None
}

fn u16_value(data: &[u8], offset: usize, little: bool) -> u16 {
    let bytes = [data[offset], data[offset + 1]];
    if little { u16::from_le_bytes(bytes) } else { u16::from_be_bytes(bytes) }
}

fn u32_value(data: &[u8], offset: usize, little: bool) -> u32 {
    let bytes = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
    if little { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) }
}
