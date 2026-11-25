//! LZMA2 chunked-stream decoding against in-code synthetic streams.
//!
//! Chunk framing (control bytes, big-endian sizes) and range-coded bodies are
//! both produced by the pure-Rust test encoder in `support`; no external
//! fixture bytes are embedded.

mod support;

use data_compression::{CodecError, CodecLimits, Lzma1, Lzma2};
use support::{Lzma1Encoder, alone, lzma2_chunk, lzma2_end, lzma2_uncompressed};

/// The lorem paragraph used by the synthetic streams.
const LOREM: &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.\n";

/// The repeated sentence used by the small streams.
const SENTENCE: &[u8] = b"Boost.rs internalized LZMA decoder fixture. ";

fn plain_text() -> Vec<u8> {
    std::iter::repeat_n(SENTENCE, 12).flatten().copied().collect()
}

fn plain_lorem() -> Vec<u8> {
    std::iter::repeat_n(LOREM, 6).flatten().copied().collect()
}

#[test]
fn decodes_single_lzma_chunk() {
    let mut stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut stream, &mut encoder, &plain_lorem(), 2, Some(0x5D));
    lzma2_end(&mut stream);
    let facts = Lzma2::decode(&stream, CodecLimits::new()).expect("single chunk decodes");
    assert_eq!(facts.value, plain_lorem());
    assert_eq!(facts.input_consumed, stream.len());
    assert_eq!(facts.trailing_input, 0);
}

#[test]
fn decodes_two_lzma_chunks() {
    let mut stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut stream, &mut encoder, &plain_lorem(), 2, Some(0x5D));
    encoder.reset_model();
    lzma2_chunk(&mut stream, &mut encoder, &plain_text(), 1, None);
    lzma2_end(&mut stream);
    let facts = Lzma2::decode(&stream, CodecLimits::new()).expect("two lzma chunks decode");
    let mut expected = plain_lorem();
    expected.extend(plain_text());
    assert_eq!(facts.value, expected);
}

#[test]
fn decodes_mixed_chunk_kinds() {
    let mut stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut stream, &mut encoder, &plain_lorem(), 2, Some(0x5D));
    lzma2_uncompressed(&mut stream, b"raw uncompressed segment 0x02 ", false);
    encoder.push_history(b"raw uncompressed segment 0x02 ");
    encoder.reset_model();
    lzma2_chunk(&mut stream, &mut encoder, &plain_text(), 1, None);
    lzma2_uncompressed(&mut stream, b"dict-reset segment 0x01 ", true);
    encoder.reset_dictionary();
    encoder.push_history(b"dict-reset segment 0x01 ");
    encoder.reset_model();
    encoder.reset_dictionary();
    let tail: Vec<u8> = std::iter::repeat_n(LOREM, 3).flatten().copied().collect();
    lzma2_chunk(&mut stream, &mut encoder, &tail, 3, Some(0x5D));
    lzma2_end(&mut stream);

    let facts = Lzma2::decode(&stream, CodecLimits::new()).expect("mixed chunk stream decodes");
    let mut expected = plain_lorem();
    expected.extend_from_slice(b"raw uncompressed segment 0x02 ");
    expected.extend(plain_text());
    expected.extend_from_slice(b"dict-reset segment 0x01 ");
    expected.extend_from_slice(&tail);
    assert_eq!(facts.value, expected);
}

#[test]
fn decodes_uncompressed_only() {
    let mut stream = Vec::new();
    lzma2_uncompressed(&mut stream, &plain_text(), true);
    lzma2_end(&mut stream);
    let facts = Lzma2::decode(&stream, CodecLimits::new()).expect("uncompressed chunk decodes");
    assert_eq!(facts.value, plain_text());
}

#[test]
fn decodes_chunk_above_sixty_four_kib() {
    let payload: Vec<u8> = std::iter::repeat_n(LOREM, 100).flatten().copied().collect();
    let mut stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut stream, &mut encoder, &payload, 2, Some(0x5D));
    lzma2_end(&mut stream);
    let facts = Lzma2::decode(&stream, CodecLimits::new()).expect("chunk size high bits decode");
    assert_eq!(facts.value, payload);
}

#[test]
fn rejects_reserved_control_byte() {
    let error = Lzma2::decode(&[0x03, 0x00, 0x00], CodecLimits::new()).expect_err("reserved control bytes are invalid");
    assert_eq!(error, CodecError::InvalidByte { offset: 0, byte: 3 });
}

#[test]
fn rejects_missing_end_marker() {
    let mut stream = Vec::new();
    lzma2_uncompressed(&mut stream, &plain_text(), true);
    let error = Lzma2::decode(&stream, CodecLimits::new()).expect_err("stream must end with the zero control byte");
    assert!(matches!(error, CodecError::InputExhausted { .. }));
    stream.push(0x00);
    assert!(Lzma2::decode(&stream, CodecLimits::new()).is_ok());
}

#[test]
fn enforces_output_limit() {
    let payload: Vec<u8> = std::iter::repeat_n(LOREM, 40).flatten().copied().collect();
    let mut stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut stream, &mut encoder, &payload, 2, Some(0x5D));
    lzma2_end(&mut stream);
    let error = Lzma2::decode(&stream, CodecLimits::new().with_max_output_bytes(1024)).expect_err("chunk exceeds the output budget");
    assert!(matches!(error, CodecError::OutputLimitExceeded { limit: 1024, .. }));
}

#[test]
fn alone_and_lzma2_agree_on_shared_payload() {
    // The same payload through both envelopes must decode to identical bytes.
    let payload = plain_text();
    let alone_stream = alone(&payload, 3, 0, 2, 1 << 22, true);
    let mut lzma2_stream = Vec::new();
    let mut encoder = Lzma1Encoder::new(3, 0, 2);
    lzma2_chunk(&mut lzma2_stream, &mut encoder, &payload, 2, Some(0x5D));
    lzma2_end(&mut lzma2_stream);
    let via_alone = Lzma1::decode(&alone_stream, CodecLimits::new()).expect("alone decodes").value;
    let via_lzma2 = Lzma2::decode(&lzma2_stream, CodecLimits::new()).expect("lzma2 decodes").value;
    assert_eq!(via_alone, via_lzma2);
    assert_eq!(via_alone, payload);
}
