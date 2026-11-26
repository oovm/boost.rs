//! Independent envelope vectors produced by the Node.js `zlib` reference encoder.
//!
//! Every byte string below was produced once with Node.js 24.18.0 from the
//! built-in `zlib` module. The test never encodes; it only decodes these fixed
//! vectors, so a bug that round-trips through this crate's own encoder cannot
//! hide behind a matching bug in the decoder.
//!
//! Reference recipe and recorded facts:
//!
//! - `RAW_HELLO`, `ZLIB_HELLO`, `GZIP_HELLO`: `zlib.deflateRawSync`,
//!   `zlib.deflateSync` and `zlib.gzipSync` of `b"Hello, DEFLATE!"` at level 9.
//!   Plaintext is 15 bytes, SHA-256
//!   `28d54d6f992f913965998453b2004df7340e8af4759af23c57482ed59f6a0026`,
//!   Adler-32 `633603159` (`0x25c40457`), CRC-32 `32427180` (`0x01eeccac`),
//!   ISIZE `15`.
//! - `RAW_EMPTY`, `ZLIB_EMPTY`, `GZIP_EMPTY`: the same three encoders of a
//!   zero-length payload at level 9. Plaintext SHA-256
//!   `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`,
//!   Adler-32 `1`, CRC-32 `0`, ISIZE `0`.
//! - `RAW_BIG`, `GZIP_BIG`: the same encoders at level 6 of
//!   `b"0123456789abcdef"` repeated 8192 times (131072 bytes, so the encoder
//!   splits the bitstream over several blocks; the raw stream is 288 bytes
//!   across 3 blocks). Plaintext SHA-256
//!   `a2706a20394e48179a86c71e82c360c2960d3652340f9b9fdb355a42e3ac7691`,
//!   Adler-32 `2490189877` (`0x946fcdf5`), CRC-32 `3671360` (`0x00380440`),
//!   ISIZE `131072`.
//! - `GZIP_HEADER_FIELDS`: `zlib.deflateRawSync` of
//!   `b"data-compression gzip header fields"` (35 bytes, SHA-256
//!   `ba125b8a9a0c77e4e47c4bbe9b1d5cbb3ba4893265421a4fa6b864c604289742`,
//!   Adler-32 `4086828404`, CRC-32 `3298751966` = `0xc4a4f59e`, ISIZE `35`)
//!   wrapped in a spec-built member header with every optional field:
//!   `FEXTRA` (SI1 `AB`, SI2 `3`, three payload bytes `de ad be`), `FNAME`
//!   `payload.txt`, `FCOMMENT` `external vector`, and `FHCRC` `0xe808`
//!   (the low 16 bits of the CRC-32 of the first 47 header bytes).
//! - `GZIP_MULTI`: `zlib.gzipSync` at level 9 of `b"first member"` followed by
//!   `zlib.gzipSync` of `b"second member"` (12 and 13 plaintext bytes, CRC-32
//!   `2318463138` and `2683991076`, ISIZE `12` and `13`). Each member is 32
//!   bytes, so the first member ends at byte 32 and 32 bytes trail it.

use data_compression::{ChecksumAlgorithm, CodecError, CodecLimits, DecodeStatus, Deflate, Envelope, Gzip, Zlib};

const HELLO: &[u8] = b"Hello, DEFLATE!";

const RAW_HELLO: &str = "f348cdc9c9d751707175f3710c71550400";
const ZLIB_HELLO: &str = "78daf348cdc9c9d751707175f3710c7155040025c40457";
const GZIP_HELLO: &str = "1f8b080000000000020af348cdc9c9d751707175f3710c71550400acccee010f000000";

const RAW_EMPTY: &str = "0300";
const ZLIB_EMPTY: &str = "78da030000000001";
const GZIP_EMPTY: &str = "1f8b080000000000020a03000000000000000000";

const RAW_BIG: &str = "edc7c901c0100000b09594bac641d97f840e22f9253c31bdb9d4d6c75cdf3ec1ddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddaffd0f";
const GZIP_BIG: &str = "1f8b080000000000000aedc7c901c0100000b09594bac641d97f840e22f9253c31bdb9d4d6c75cdf3ec1ddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddaffd0f4005380000000200";

const GZIP_HEADER_FIELDS: &str = "1f8b081e112233440003070041420300deadbe7061796c6f61642e7478740065787465726e616c20766563746f720008e84b492c49d44dcecf2d284a2d2ececccf5348afca2c50c8484d4c492d5248cb4ccd49290600def59ec423000000";

const GZIP_MULTI: &str =
    "1f8b080000000000020a4bcb2c2a2e51c84dcd4d4a2d0200a2f0308a0c0000001f8b080000000000020a2b4e4dcecf4b51c84dcd4d4a2d02002474fa9f0d000000";

/// Decodes a hex literal into bytes.
fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "hex literal has an odd length");
    (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex digit")).collect()
}

/// Plaintext of the `GZIP_HEADER_FIELDS` member.
const HEADER_PLAIN: &[u8] = b"data-compression gzip header fields";

/// Plaintext of the big multi-block vectors.
fn big_payload() -> Vec<u8> {
    b"0123456789abcdef".repeat(8192)
}

/// Limits with only an output cap, matching how the products bound a decode.
fn capped(limit: u64) -> CodecLimits {
    CodecLimits::new().with_max_output_bytes(limit)
}

#[test]
fn raw_reference_vector_decodes_completely() {
    let vector = unhex(RAW_HELLO);
    let decoded = Deflate::decode_bounded(&vector, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.consumed, vector.len());
    assert_eq!(decoded.trailing_input, 0);
    assert_eq!(decoded.status, DecodeStatus::Complete);
    assert!(decoded.checksum.is_none(), "raw DEFLATE has no trailer");
}

#[test]
fn zlib_reference_vector_reports_adler32_and_consumed_bytes() {
    let vector = unhex(ZLIB_HELLO);
    let decoded = Zlib::decode_bounded(&vector, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.consumed, vector.len());
    assert_eq!(decoded.trailing_input, 0);
    assert_eq!(decoded.status, DecodeStatus::Complete);
    let checksum = decoded.checksum.expect("zlib checksum facts");
    assert_eq!(checksum.algorithm, ChecksumAlgorithm::Adler32);
    assert_eq!(checksum.value, 633_603_159);
    assert!(checksum.verified);
}

#[test]
fn gzip_reference_vector_reports_crc32_and_consumed_bytes() {
    let vector = unhex(GZIP_HELLO);
    let decoded = Gzip::decode_bounded(&vector, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.consumed, vector.len());
    assert_eq!(decoded.trailing_input, 0);
    assert_eq!(decoded.status, DecodeStatus::Complete);
    let checksum = decoded.checksum.expect("gzip checksum facts");
    assert_eq!(checksum.algorithm, ChecksumAlgorithm::Crc32);
    assert_eq!(checksum.value, 32_427_180);
    assert!(checksum.verified);
}

#[test]
fn empty_payloads_complete_in_every_envelope() {
    let raw = Deflate::decode_bounded(&unhex(RAW_EMPTY), CodecLimits::new());
    assert_eq!(raw.output, b"");
    assert_eq!(raw.status, DecodeStatus::Complete);
    assert_eq!(raw.consumed, unhex(RAW_EMPTY).len());

    let zlib = Zlib::decode_bounded(&unhex(ZLIB_EMPTY), CodecLimits::new());
    assert_eq!(zlib.output, b"");
    assert_eq!(zlib.status, DecodeStatus::Complete);
    assert_eq!(zlib.consumed, unhex(ZLIB_EMPTY).len());
    assert_eq!(zlib.checksum.expect("adler32").value, 1);

    let gzip = Gzip::decode_bounded(&unhex(GZIP_EMPTY), CodecLimits::new());
    assert_eq!(gzip.output, b"");
    assert_eq!(gzip.status, DecodeStatus::Complete);
    assert_eq!(gzip.consumed, unhex(GZIP_EMPTY).len());
    assert_eq!(gzip.checksum.expect("crc32").value, 0);
}

#[test]
fn envelope_detection_picks_the_reference_envelopes() {
    assert_eq!(Envelope::detect(&unhex(GZIP_HELLO)), Envelope::Gzip);
    assert_eq!(Envelope::detect(&unhex(ZLIB_HELLO)), Envelope::Zlib);
    assert_eq!(Envelope::detect(&unhex(RAW_HELLO)), Envelope::Raw);
    assert_eq!(Envelope::detect(&[]), Envelope::Raw);

    for (envelope, vector) in [(Envelope::Raw, RAW_HELLO), (Envelope::Zlib, ZLIB_HELLO), (Envelope::Gzip, GZIP_HELLO)] {
        let decoded = envelope.decode(&unhex(vector), CodecLimits::new());
        assert_eq!(decoded.output, HELLO, "{} dispatch", envelope.as_str());
        assert!(decoded.is_complete(), "{} dispatch", envelope.as_str());
    }
}

#[test]
fn trailing_bytes_are_reported_not_consumed() {
    for (envelope, vector) in [(Envelope::Raw, RAW_HELLO), (Envelope::Zlib, ZLIB_HELLO), (Envelope::Gzip, GZIP_HELLO)] {
        let mut input = unhex(vector);
        let member_len = input.len();
        input.extend_from_slice(b"TRAIL");
        let decoded = envelope.decode(&input, CodecLimits::new());
        assert_eq!(decoded.output, HELLO);
        assert_eq!(decoded.status, DecodeStatus::Complete);
        assert_eq!(decoded.consumed, member_len, "{} consumed", envelope.as_str());
        assert_eq!(decoded.trailing_input, 5, "{} trailing", envelope.as_str());
    }
}

#[test]
fn truncated_input_keeps_the_decoded_prefix() {
    let vector = unhex(RAW_HELLO);
    let decoded = Deflate::decode_bounded(&vector[..10], CodecLimits::new());
    assert_eq!(decoded.status, DecodeStatus::InputTruncated { offset: 10, needed: 1 });
    assert_eq!(decoded.consumed, 10);
    assert_eq!(decoded.output, &HELLO[..decoded.output.len()]);

    let vector = unhex(ZLIB_HELLO);
    let body = &vector[..vector.len() - 2];
    let decoded = Zlib::decode_bounded(body, CodecLimits::new());
    assert_eq!(decoded.output, HELLO, "the bitstream itself is complete");
    assert_eq!(decoded.status, DecodeStatus::InputTruncated { offset: body.len(), needed: 2 });
    assert_eq!(decoded.consumed, body.len());

    let vector = unhex(GZIP_HELLO);
    let body = &vector[..vector.len() - 4];
    let decoded = Gzip::decode_bounded(body, CodecLimits::new());
    assert_eq!(decoded.output, HELLO, "the bitstream itself is complete");
    assert_eq!(decoded.status, DecodeStatus::InputTruncated { offset: body.len(), needed: 4 });
    assert_eq!(decoded.consumed, body.len());
}

#[test]
fn corrupt_zlib_checksum_is_a_typed_status() {
    let vector = unhex(ZLIB_HELLO);
    let mut corrupted = vector.clone();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xFF;
    let decoded = Zlib::decode_bounded(&corrupted, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.consumed, vector.len());
    assert_eq!(
        decoded.status,
        DecodeStatus::ChecksumMismatch { algorithm: ChecksumAlgorithm::Adler32, expected: 633_603_240, actual: 633_603_159 }
    );
    let checksum = decoded.checksum.expect("checksum facts survive the mismatch");
    assert!(!checksum.verified);
}

#[test]
fn corrupt_gzip_crc_is_a_typed_status() {
    let vector = unhex(GZIP_HELLO);
    let mut corrupted = vector.clone();
    let crc_at = corrupted.len() - 8;
    corrupted[crc_at] ^= 0xFF;
    let decoded = Gzip::decode_bounded(&corrupted, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.consumed, vector.len());
    assert!(matches!(decoded.status, DecodeStatus::ChecksumMismatch { algorithm: ChecksumAlgorithm::Crc32, .. }));
    assert_eq!(decoded.status.code(), "compression/checksum-mismatch");
}

#[test]
fn gzip_isize_mismatch_is_a_length_status() {
    let vector = unhex(GZIP_HELLO);
    let mut corrupted = vector.clone();
    let isize_at = corrupted.len() - 4;
    corrupted[isize_at] ^= 0xFF;
    let decoded = Gzip::decode_bounded(&corrupted, CodecLimits::new());
    assert_eq!(decoded.output, HELLO);
    assert_eq!(decoded.status, DecodeStatus::LengthMismatch { announced: u64::from(15u32 ^ 0xFF), actual: 15 });
    assert_eq!(decoded.status.code(), "compression/length-mismatch");
}

#[test]
fn gzip_optional_header_fields_are_skipped_and_verified() {
    let vector = unhex(GZIP_HEADER_FIELDS);
    let decoded = Gzip::decode_bounded(&vector, CodecLimits::new());
    assert_eq!(decoded.output, HEADER_PLAIN);
    assert_eq!(decoded.status, DecodeStatus::Complete);
    assert_eq!(decoded.consumed, vector.len());
    assert_eq!(decoded.checksum.expect("crc32").value, 3_298_751_966);

    // A broken FHCRC must be rejected before any payload byte is emitted.
    let mut broken = vector.clone();
    broken[47] ^= 0xFF;
    let decoded = Gzip::decode_bounded(&broken, CodecLimits::new());
    assert!(decoded.output.is_empty());
    assert_eq!(decoded.consumed, 0);
    assert!(matches!(decoded.status, DecodeStatus::ChecksumMismatch { algorithm: ChecksumAlgorithm::Crc32, .. }));
}

#[test]
fn gzip_multi_member_decodes_only_the_first_member() {
    let vector = unhex(GZIP_MULTI);
    assert_eq!(vector.len(), 65, "a 32-byte first member plus a 33-byte second member");
    let decoded = Gzip::decode_bounded(&vector, CodecLimits::new());
    assert_eq!(decoded.output, b"first member");
    assert_eq!(decoded.status, DecodeStatus::Complete);
    assert_eq!(decoded.consumed, 32);
    assert_eq!(decoded.trailing_input, 33);
    assert_eq!(decoded.checksum.expect("first member crc").value, 2_318_463_138);
}

#[test]
fn multi_block_payload_decodes_completely() {
    let payload = big_payload();
    let decoded = Deflate::decode_bounded(&unhex(RAW_BIG), CodecLimits::new());
    assert_eq!(decoded.output, payload);
    assert_eq!(decoded.consumed, unhex(RAW_BIG).len());
    assert!(decoded.is_complete());

    let decoded = Gzip::decode_bounded(&unhex(GZIP_BIG), CodecLimits::new());
    assert_eq!(decoded.output, payload);
    assert_eq!(decoded.consumed, unhex(GZIP_BIG).len());
    assert_eq!(decoded.checksum.expect("crc32").value, 3_671_360);
}

#[test]
fn output_limit_keeps_the_partial_prefix() {
    let vector = unhex(RAW_BIG);
    let payload = big_payload();
    // A back-reference longer than the remaining room is refused whole, so the
    // prefix can be shorter than the cap but never crosses it.
    let decoded = Deflate::decode_bounded(&vector, capped(1000));
    assert_eq!(decoded.output.len(), 791);
    assert_eq!(decoded.output, &payload[..791]);
    assert_eq!(decoded.status, DecodeStatus::OutputLimit { produced: 791, limit: 1000 });
    assert_eq!(decoded.consumed, 34);

    // The envelope header is consumed before the cap applies.
    let vector = unhex(ZLIB_HELLO);
    let decoded = Zlib::decode_bounded(&vector, capped(1));
    assert_eq!(decoded.output, b"H");
    assert_eq!(decoded.status, DecodeStatus::OutputLimit { produced: 1, limit: 1 });
    assert_eq!(decoded.consumed, 5);

    let vector = unhex(GZIP_HELLO);
    let decoded = Gzip::decode_bounded(&vector, capped(1));
    assert_eq!(decoded.output, b"H");
    assert_eq!(decoded.status, DecodeStatus::OutputLimit { produced: 1, limit: 1 });
    assert_eq!(decoded.consumed, 13);
}

#[test]
fn stable_error_codes_do_not_depend_on_messages() {
    assert_eq!(CodecError::InputExhausted { offset: 1, needed: 2 }.code(), "compression/input-exhausted");
    assert_eq!(CodecError::OutputLimitExceeded { produced: 5, limit: 5 }.code(), "compression/output-limit");
    assert_eq!(CodecError::LengthMismatch { announced: 4, actual: 3 }.code(), "compression/length-mismatch");
    assert_eq!(DecodeStatus::Complete.code(), "compression/complete");

    let invalid = Deflate::decode_bounded(&[0xFF, 0xFF, 0xFF], CodecLimits::new());
    assert_eq!(invalid.status.code(), "compression/unsupported-feature");
    assert!(matches!(invalid.status, DecodeStatus::Invalid { .. }));
}
