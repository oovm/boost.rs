//! Brotli (RFC 7932) decoding against fixtures produced with Node.js `zlib`.
//!
//! Every compressed byte string lives in `fixtures/mod.rs` and was produced once
//! with Node.js 24 `zlib.brotliCompressSync`: quality 0 and 5 for the short
//! ASCII payload, quality 5 for the binary payload, quality 11 for the
//! repetitive, text-mode, mixed, distant-match and postfix-heavy corpora, and
//! `LGWIN10` with window size 10. The tests never encode, so a matching bug in
//! this crate's own encoder cannot hide a decoder bug.

mod fixtures;

use data_compression::{
    Brotli, CodecError, CodecLimits, DecodeStatus,
    brotli::{apply_transform, word},
};
use fixtures::{BINARY, DISTANT, EMPTY, LGWIN10, MIXED, POSTFIXED, REPETITIVE, SHORT_Q0, SHORT_Q5, TEXT, TRUNCATED};

const WORDS: [&str; 8] = ["brotli", "static", "dictionary", "compress", "window", "context", "literal", "command"];

const PARAGRAPH: &str = "Brotli is a generic-purpose lossless compression algorithm that compresses data using a combination of a modern variant of the LZ77 algorithm, Huffman coding and second-order context modeling, providing a compression ratio comparable to the best currently available general-purpose compression methods. It is similar in speed with deflate but offers more dense compression. The specification of the Brotli compressed data format does not define the encoder, only the decoder. ";

const VOCABULARY: [&str; 22] = [
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "theta",
    "lambda",
    "literal",
    "distance",
    "metablock",
    "huffman",
    "prefix",
    "suffix",
    "window",
    "transform",
    "dictionary",
    "context",
    "command",
    "insert",
    "copy",
    "block",
];

/// Decodes a hex-encoded compressed fixture.
fn compressed(value: &str) -> Vec<u8> {
    assert!(value.len() % 2 == 0, "odd number of hex digits");
    (0..value.len()).step_by(2).map(|index| u8::from_str_radix(&value[index..index + 2], 16).expect("valid hex")).collect()
}

/// Limits with only an output cap, matching how the products bound a decode.
fn capped(limit: usize) -> CodecLimits {
    CodecLimits::new().with_max_output_bytes(limit as u64)
}

/// One-shot decode with an output cap, mirroring `Brotli::decode`.
fn decode(bytes: &[u8], limit: usize) -> Result<Vec<u8>, CodecError> {
    Brotli::decode(bytes, capped(limit))
}

fn short_plaintext() -> &'static [u8] {
    b"Hello, Brotli! This is a short ASCII fixture."
}

fn repetitive_plaintext() -> Vec<u8> {
    let mut text = String::new();
    for index in 0..20_000usize {
        text.push_str(WORDS[index % WORDS.len()]);
        text.push(if index % 7 == 0 { '\n' } else { ' ' });
    }
    text.into_bytes()
}

fn binary_plaintext() -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    let mut bytes = Vec::with_capacity(5000);
    for _ in 0..5000 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        bytes.push((state >> 24) as u8);
    }
    bytes
}

fn text_plaintext() -> Vec<u8> {
    let mut text = String::new();
    for _ in 0..40 {
        text.push_str(PARAGRAPH);
    }
    text.into_bytes()
}

fn mixed_plaintext() -> Vec<u8> {
    let mut text = String::new();
    let mut seed: u32 = 12_345;
    for index in 0..90_000u32 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        text.push_str(VOCABULARY[(seed % VOCABULARY.len() as u32) as usize]);
        text.push_str(if seed % 11 == 0 { ", " } else { " " });
        if index % 13 == 0 {
            text.push_str(&format!("{}\n", seed % 100_000));
        }
    }
    text.into_bytes()
}

fn distant_plaintext() -> Vec<u8> {
    let mut fragments: Vec<String> = Vec::with_capacity(1200);
    let mut seed: u32 = 999;
    for _ in 0..1200 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        fragments.push(format!(
            "{} {} {} ",
            VOCABULARY[(seed % VOCABULARY.len() as u32) as usize],
            VOCABULARY[((seed >> 8) % VOCABULARY.len() as u32) as usize],
            seed % 99_991
        ));
    }
    let mut text = String::new();
    for index in 0..12_000u32 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        text.push_str(&fragments[(seed % 1200) as usize]);
        if index % 17 == 0 {
            text.push('\n');
        }
    }
    text.into_bytes()
}

fn postfixed_plaintext() -> Vec<u8> {
    let mut buffer = vec![0u8; 400 * 1024];
    let mut seed: u32 = 777;
    for byte in buffer.iter_mut() {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        *byte = ((seed >> 16) & 0xff) as u8;
    }
    for block in 1..97usize {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let source = (seed % block as u32) as usize * 4096;
        let start = block * 4096;
        let chunk = buffer[source..source + 4096].to_vec();
        buffer[start..start + 4096].copy_from_slice(&chunk);
    }
    buffer
}

fn windowed_plaintext() -> Vec<u8> {
    let mut text = String::new();
    for index in 0..400usize {
        text.push_str(&format!("line {index}: {} {}\n", WORDS[index % WORDS.len()], index * 7));
    }
    text.into_bytes()
}

#[test]
fn empty_stream_decodes_to_nothing() {
    let output = decode(&compressed(EMPTY), 0).expect("empty stream");
    assert!(output.is_empty());
}

#[test]
fn missing_input_is_truncated() {
    assert_eq!(decode(&[], 16), Err(CodecError::InputExhausted { offset: 0, needed: 1 }));
}

#[test]
fn short_ascii_at_quality_zero() {
    let output = decode(&compressed(SHORT_Q0), 1024).expect("quality 0 stream");
    assert_eq!(output, short_plaintext());
}

#[test]
fn short_ascii_at_quality_five() {
    let output = decode(&compressed(SHORT_Q5), 1024).expect("quality 5 stream");
    assert_eq!(output, short_plaintext());
}

#[test]
fn repetitive_text_at_quality_eleven() {
    let output = decode(&compressed(REPETITIVE), 1 << 20).expect("quality 11 stream");
    assert_eq!(output, repetitive_plaintext());
}

#[test]
fn binary_data_at_quality_five() {
    let output = decode(&compressed(BINARY), 8192).expect("binary stream");
    assert_eq!(output, binary_plaintext());
}

#[test]
fn text_mode_stream_at_quality_eleven() {
    let output = decode(&compressed(TEXT), 1 << 20).expect("text mode stream");
    assert_eq!(output, text_plaintext());
}

#[test]
fn window_size_ten() {
    let output = decode(&compressed(LGWIN10), 1 << 20).expect("window 10 stream");
    assert_eq!(output, windowed_plaintext());
}

#[test]
fn mixed_corpus_with_block_switching() {
    let output = decode(&compressed(MIXED), 4 << 20).expect("mixed corpus");
    assert_eq!(output, mixed_plaintext());
}

#[test]
fn distant_matches_with_large_window() {
    let output = decode(&compressed(DISTANT), 4 << 20).expect("distant corpus");
    assert_eq!(output, distant_plaintext());
}

#[test]
fn distance_parameter_heavy_stream() {
    let output = decode(&compressed(POSTFIXED), 4 << 20).expect("postfix corpus");
    assert_eq!(output, postfixed_plaintext());
}

#[test]
fn truncated_stream_reports_truncation() {
    let result = decode(&compressed(TRUNCATED), 1 << 20);
    assert!(matches!(result, Err(CodecError::InputExhausted { .. })));
}

#[test]
fn corrupt_meta_block_length_is_rejected() {
    // WBITS=16, ISLAST=0, five length nibbles of which the top one is zero.
    let input = [0x04, 0x00, 0x00, 0x00];
    assert!(matches!(decode(&input, 1024), Err(CodecError::InvalidBitPattern { .. })));
}

#[test]
fn corrupted_byte_is_rejected() {
    let mut bytes = compressed(REPETITIVE);
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    assert!(decode(&bytes, 1 << 20).is_err());
}

#[test]
fn limit_smaller_than_output_is_rejected() {
    let expected = short_plaintext();
    let limit = expected.len() - 1;
    let result = decode(&compressed(SHORT_Q5), limit);
    assert!(matches!(result, Err(CodecError::OutputLimitExceeded { limit: reported, .. }) if reported == limit as u64));
}

#[test]
fn exact_limit_is_accepted() {
    let expected = short_plaintext();
    let output = decode(&compressed(SHORT_Q5), expected.len()).expect("exact limit");
    assert_eq!(output, expected);
}

#[test]
fn limit_zero_for_empty_output_is_accepted() {
    assert!(decode(&compressed(EMPTY), 0).is_ok());
}

#[test]
fn trailing_bytes_are_ignored() {
    let mut bytes = compressed(SHORT_Q5);
    bytes.extend_from_slice(&[0xff; 32]);
    let output = decode(&bytes, 1024).expect("trailing bytes");
    assert_eq!(output, short_plaintext());
}

#[test]
fn identity_transform_keeps_the_word() {
    assert_eq!(apply_transform(0, b"time").unwrap(), b"time");
}

#[test]
fn suffix_transform_appends_a_space() {
    assert_eq!(apply_transform(1, b"time").unwrap(), b"time ");
}

#[test]
fn uppercase_first_transform() {
    assert_eq!(apply_transform(4, b"time").unwrap(), b"Time ");
}

#[test]
fn omit_first_and_last_transforms() {
    assert_eq!(apply_transform(3, b"time").unwrap(), b"ime");
    assert_eq!(apply_transform(12, b"time").unwrap(), b"tim");
}

#[test]
fn dictionary_lookup_of_the_first_word() {
    assert_eq!(word(4, 0).unwrap(), b"time");
}

#[test]
fn dictionary_rejects_unknown_lengths() {
    assert!(word(3, 0).is_err());
    assert!(word(25, 0).is_err());
}

#[test]
fn bounded_output_limit_keeps_a_valid_prefix() {
    let expected = repetitive_plaintext();
    let bounded = Brotli::decode_bounded(&compressed(REPETITIVE), capped(1000));
    let DecodeStatus::OutputLimit { produced, limit } = bounded.status
    else {
        panic!("expected an output-limit stop, got {:?}", bounded.status);
    };
    assert_eq!(limit, 1000);
    assert_eq!(produced, bounded.output.len() as u64);
    assert!(bounded.output.len() <= 1000);
    assert_eq!(bounded.output, expected[..bounded.output.len()]);
    assert_eq!(bounded.trailing_input, 0);
    assert!(bounded.checksum.is_none());
}

#[test]
fn bounded_truncated_stream_reports_input_truncated() {
    let bounded = Brotli::decode_bounded(&compressed(TRUNCATED), capped(1 << 20));
    assert!(matches!(bounded.status, DecodeStatus::InputTruncated { .. }));
    assert_eq!(bounded.trailing_input, 0);
}

#[test]
fn bounded_corrupt_stream_reports_invalid() {
    let mut bytes = compressed(REPETITIVE);
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    let bounded = Brotli::decode_bounded(&bytes, capped(1 << 20));
    assert!(matches!(bounded.status, DecodeStatus::Invalid { .. }));
    assert!(bounded.consumed <= bytes.len());
}

#[test]
fn bounded_reports_trailing_bytes() {
    let mut bytes = compressed(SHORT_Q5);
    bytes.extend_from_slice(&[0xff; 32]);
    let bounded = Brotli::decode_bounded(&bytes, capped(1024));
    assert!(bounded.is_complete());
    assert_eq!(bounded.output, short_plaintext());
    assert_eq!(bounded.trailing_input, 32);
    assert_eq!(bounded.consumed + bounded.trailing_input, bytes.len());
}

#[test]
fn bounded_empty_stream_is_complete() {
    let bytes = compressed(EMPTY);
    let bounded = Brotli::decode_bounded(&bytes, capped(0));
    assert!(bounded.is_complete());
    assert!(bounded.output.is_empty());
    assert_eq!(bounded.consumed, bytes.len());
    assert_eq!(bounded.trailing_input, 0);
}

#[test]
fn bounded_empty_input_is_input_truncated() {
    let bounded = Brotli::decode_bounded(&[], capped(16));
    assert_eq!(bounded.status, DecodeStatus::InputTruncated { offset: 0, needed: 1 });
    assert_eq!(bounded.consumed, 0);
    assert!(bounded.output.is_empty());
}
