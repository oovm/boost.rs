//! LZMA1 (`LZMA_ALONE` and raw body) decoding against in-code synthetic streams.
//!
//! Every encoded byte is produced by the pure-Rust test encoder in `support`,
//! which mirrors the decoder model symbol for symbol. No external fixture
//! bytes are embedded.

mod support;

use data_compression::{CodecError, CodecFeature, CodecLimits, Lzma1};
use support::{Lzma1Encoder, alone};

/// The lorem paragraph used by the synthetic streams.
const LOREM: &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.\n";

/// The repeated sentence used by the small streams.
const SENTENCE: &[u8] = b"Boost.rs internalized LZMA decoder fixture. ";

/// Deterministic pseudo-random bytes.
fn lcg_bytes(count: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..count)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 16) & 0xFF) as u8
        })
        .collect()
}

/// A payload whose repeats only match tens of kilobytes back, exercising the
/// direct and align distance models.
fn long_distance() -> Vec<u8> {
    let head = lcg_bytes(2048);
    let mut out = head.clone();
    out.extend_from_slice(LOREM);
    out.extend(std::iter::repeat_n(b'B', 20_000));
    out.extend((0..15_000u32).map(|i| (i % 251) as u8));
    out.extend_from_slice(LOREM);
    out.extend_from_slice(&head);
    out.extend(std::iter::repeat_n(LOREM, 4).flatten().copied());
    out
}

fn plain_text() -> Vec<u8> {
    std::iter::repeat_n(SENTENCE, 12).flatten().copied().collect()
}

fn plain_lorem() -> Vec<u8> {
    std::iter::repeat_n(LOREM, 6).flatten().copied().collect()
}

#[test]
fn decodes_alone_known_size() {
    let stream = alone(&plain_text(), 3, 0, 2, 1 << 22, true);
    let facts = Lzma1::decode(&stream, CodecLimits::new()).expect("default properties decode");
    assert_eq!(facts.value, plain_text());
    assert_eq!(facts.output_produced, plain_text().len());
    assert!(facts.complete);
    assert_eq!(facts.input_consumed + facts.trailing_input, stream.len());
}

#[test]
fn decodes_alone_unknown_size() {
    let stream = alone(&plain_lorem(), 3, 0, 2, 1 << 22, false);
    let facts = Lzma1::decode(&stream, CodecLimits::new()).expect("end marker terminates the body");
    assert_eq!(facts.value, plain_lorem());
}

#[test]
fn encoding_is_deterministic() {
    let first = alone(&plain_lorem(), 3, 0, 2, 1 << 22, true);
    let second = alone(&plain_lorem(), 3, 0, 2, 1 << 22, true);
    assert_eq!(first, second);
}

#[test]
fn decodes_property_combinations() {
    for (lc, lp, pb) in [(0, 0, 0), (1, 2, 1), (4, 0, 2), (2, 1, 0)] {
        let stream = alone(&plain_lorem(), lc, lp, pb, 1 << 22, true);
        let facts = Lzma1::decode(&stream, CodecLimits::new()).expect("property combination decodes");
        assert_eq!(facts.value, plain_lorem());
    }
}

#[test]
fn decodes_long_distance_matches() {
    let stream = alone(&long_distance(), 3, 0, 2, 1 << 22, true);
    let facts = Lzma1::decode(&stream, CodecLimits::new()).expect("long-distance stream decodes");
    assert_eq!(facts.value, long_distance());
}

#[test]
fn decodes_empty_payload() {
    let stream = alone(&[], 3, 0, 2, 1 << 22, true);
    let facts = Lzma1::decode(&stream, CodecLimits::new()).expect("empty payload decodes");
    assert!(facts.value.is_empty());
    assert_eq!(facts.output_produced, 0);
}

#[test]
fn decode_raw_matches_envelope() {
    let stream = alone(&plain_text(), 3, 0, 2, 1 << 22, true);
    let facts = Lzma1::decode_raw(
        &stream[..5].try_into().expect("five property bytes"),
        &stream[13..],
        Some(plain_text().len() as u64),
        CodecLimits::new(),
    )
    .expect("raw body decodes");
    assert_eq!(facts.value, plain_text());
}

#[test]
fn rejects_invalid_property_byte() {
    let mut stream = alone(&plain_text(), 3, 0, 2, 1 << 22, true);
    stream[0] = 225;
    let error = Lzma1::decode(&stream, CodecLimits::new()).expect_err("pb above four is invalid");
    assert_eq!(error, CodecError::InvalidByte { offset: 0, byte: 225 });
}

#[test]
fn rejects_literal_context_overflow() {
    // `lc = 4, lp = 1` sums above the four-context reference limit.
    let mut stream = Vec::new();
    stream.push(4 + 9 + 2 * 45);
    stream.extend((1u32 << 20).to_le_bytes());
    stream.extend((1u64).to_le_bytes());
    let mut encoder = Lzma1Encoder::new(4, 0, 2);
    encoder.encode(b"x");
    stream.extend(encoder.take_body());
    let error = Lzma1::decode(&stream, CodecLimits::new()).expect_err("lc plus lp above four is invalid");
    assert_eq!(error, CodecError::InvalidByte { offset: 0, byte: 4 + 9 + 2 * 45 });
}

#[test]
fn rejects_oversized_dictionary() {
    let mut stream = alone(&plain_text(), 3, 0, 2, 1 << 22, true);
    stream[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
    let error = Lzma1::decode(&stream, CodecLimits::new()).expect_err("dictionary above one gibibyte is refused");
    assert_eq!(error, CodecError::UnsupportedFeature { feature: CodecFeature::DictionarySize(u32::MAX) });
}

#[test]
fn enforces_output_limit() {
    let stream = alone(&long_distance(), 3, 0, 2, 1 << 22, true);
    let error = Lzma1::decode(&stream, CodecLimits::new().with_max_output_bytes(1024)).expect_err("payload exceeds the output budget");
    assert!(matches!(error, CodecError::OutputLimitExceeded { limit: 1024, .. }));
}

#[test]
fn rejects_truncated_body() {
    let mut stream = alone(&long_distance(), 3, 0, 2, 1 << 22, true);
    stream.truncate(stream.len() - 40);
    let error = Lzma1::decode(&stream, CodecLimits::new()).expect_err("announced size cannot be reached");
    assert!(matches!(error, CodecError::InputExhausted { .. }));
}
