//! PNG writer guarantees: stable bytes, a lossless round trip, and rejection
//! of a payload whose chunk CRC no longer matches.

use media_codec::encode_png;
use media_image::{DecodeError, DecodeLimits, decode_png};

/// A 2×2 RGBA fixture with one fully transparent pixel.
const FIXTURE_PIXELS: [u8; 16] = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 128, 255, 255, 255, 0];

/// The exact bytes the writer produced for [`FIXTURE_PIXELS`] before the
/// checksum and Deflate primitives moved to their owning crates.
const FIXTURE_PNG: [u8; 86] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
    0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xB6, 0x0D, 0x24, 0x00, 0x00, 0x00,
    0x1D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x01, 0x01, 0x12, 0x00, 0xED, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00,
    0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0x80, 0xFF, 0xFF, 0xFF, 0x00, 0x46, 0x4E, 0x08, 0x79, 0xE6, 0x15,
    0x75, 0x91, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[test]
fn written_png_is_byte_identical_to_the_fixture() {
    let written = encode_png(2, 2, &FIXTURE_PIXELS).expect("encode fixture");
    assert_eq!(written, FIXTURE_PNG);
}

#[test]
fn rgba_round_trip_preserves_pixels() {
    let written = encode_png(2, 2, &FIXTURE_PIXELS).expect("encode fixture");
    let decoded = decode_png(&written, DecodeLimits::unbounded()).expect("decode fixture");
    assert_eq!(decoded.bitmap.width, 2);
    assert_eq!(decoded.bitmap.height, 2);
    assert_eq!(decoded.bitmap.rgba, FIXTURE_PIXELS);
}

#[test]
fn flipped_chunk_crc_is_detected() {
    let mut corrupted = encode_png(2, 2, &FIXTURE_PIXELS).expect("encode fixture");
    // Signature is 8 bytes, then IHDR's length/type/data occupy 8..29, so the
    // last byte of the IHDR CRC sits at index 32.
    corrupted[32] ^= 0xFF;
    match decode_png(&corrupted, DecodeLimits::unbounded()) {
        Err(DecodeError::ChunkCrc { kind, .. }) => assert_eq!(&kind, b"IHDR"),
        other => panic!("expected an IHDR CRC failure, got {other:?}"),
    }
}
