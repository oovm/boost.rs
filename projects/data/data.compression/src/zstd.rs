//! Decodes a single dictionary-free Zstandard frame, as used by MongoDB.
//!
//! The private block and entropy decoder is adapted from the MIT-licensed
//! ruzstd 0.9.1 (zstd-rs commit aa145e8757c6982aab6a8336d3bba43d1a4c6fae).
//! See zstd/LICENSE-MIT. The port uses std I/O and the shared Boost checksum.

// Upstream debug traces are not part of the decoder's output.
macro_rules! vprintln {
    ($($arg:tt)*) => {{}};
}

#[allow(dead_code)]
mod bit_io;
mod blocks;
mod common;
#[allow(dead_code)]
mod decoding;
mod fse;
mod huff0;

use crate::CompressionError;
use std::io::Read;

/// Decodes one Zstandard frame, validating the envelope size and any frame checksum.
pub fn decode(input: &[u8], announced: usize) -> Result<Vec<u8>, CompressionError> {
    let mut decoder = decoding::StreamingDecoder::new(input).map_err(|_| CompressionError::Invalid)?;
    let mut output = Vec::with_capacity(announced.min(1 << 20));
    decoder.by_ref().take((announced as u64).saturating_add(1)).read_to_end(&mut output).map_err(|_| CompressionError::Invalid)?;
    if output.len() != announced {
        return Err(CompressionError::LengthMismatch { announced, actual: output.len() });
    }
    let (remaining, frame) = decoder.into_parts();
    if !remaining.is_empty() || !frame.is_finished() {
        return Err(CompressionError::Invalid);
    }
    if let Some(expected) = frame.get_checksum_from_data() {
        if frame.get_calculated_checksum() != Some(expected) {
            return Err(CompressionError::Invalid);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("zstd_vectors.rs");

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex")).collect()
    }

    #[test]
    fn decodes_frames_produced_by_independent_libzstd() {
        for (name, original_hex, compressed_hex) in KNOWN {
            let original = hex_bytes(original_hex);
            let compressed = hex_bytes(compressed_hex);
            let decoded = decode(&compressed, original.len()).unwrap_or_else(|err| panic!("decode {name}: {err:?}"));
            assert_eq!(decoded, original, "mismatch for {name}");
        }
    }

    #[test]
    fn decodes_large_zero_filled_frames() {
        for (len, compressed_hex) in ZEROES {
            let original = vec![0u8; *len];
            let compressed = hex_bytes(compressed_hex);
            let decoded = decode(&compressed, original.len()).expect("decode");
            assert_eq!(decoded.len(), original.len());
            assert!(decoded.iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn rejects_wrong_announced_length() {
        let (_, original_hex, compressed_hex) = &KNOWN[1]; // "hello"
        let original = hex_bytes(original_hex);
        let compressed = hex_bytes(compressed_hex);
        let wrong = original.len() + 1;
        assert!(matches!(
            decode(&compressed, wrong),
            Err(CompressionError::LengthMismatch { announced, actual }) if announced == wrong && actual == original.len()
        ));
    }

    #[test]
    fn rejects_truncated_or_malformed_stream() {
        // 魔数截断。
        assert_eq!(decode(&[0x28, 0xb5], 0), Err(CompressionError::Invalid));
        // 非 zstd 魔数。
        assert_eq!(decode(&[0x00, 0x00, 0x00, 0x00], 0), Err(CompressionError::Invalid));
    }

    #[test]
    fn validates_the_frame_checksum_and_rejects_trailing_bytes() {
        // Single-segment raw block containing "hello", with low 32 bits of
        // the published XXH64("hello") vector 0x26c7827d889f6da3.
        let mut frame = hex_bytes("28b52ffd240529000068656c6c6fa36d9f88");
        assert_eq!(decode(&frame, 5), Ok(b"hello".to_vec()));
        frame.push(0);
        assert_eq!(decode(&frame, 5), Err(CompressionError::Invalid));
        frame.pop();
        frame[13] ^= 1;
        assert_eq!(decode(&frame, 5), Err(CompressionError::Invalid));
    }

    #[test]
    fn rejects_output_beyond_the_envelope_budget() {
        let (len, compressed) = ZEROES[1];
        assert!(len > 10);
        assert_eq!(decode(&hex_bytes(compressed), 10), Err(CompressionError::LengthMismatch { announced: 10, actual: 11 }));
    }
}
