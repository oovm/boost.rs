use data_encoding::{Canonicality, EncodingError, SliceCodec, representation::Base64};

#[test]
fn base64_round_trip() {
    let codec = Base64::standard();
    let inputs: &[&[u8]] = &[&[], b"f", b"fo", b"foo", b"foobar", b"data-encoding"];
    for input in inputs {
        let encoded = codec.encode(input);
        let decoded = codec.decode(&encoded).expect("decode");
        assert_eq!(decoded.value.as_slice(), *input);
        assert_eq!(decoded.canonical, Canonicality::Canonical);
    }
}

#[test]
fn base64_known_vectors() {
    let codec = Base64::standard();
    assert_eq!(codec.encode(b"Man"), b"TWFu");
    assert_eq!(codec.decode(b"TWFu").expect("decode").value, b"Man");
    assert_eq!(codec.decode(b"").expect("empty").value, b"");
}

#[test]
fn base64_rejects_invalid_alphabet() {
    let codec = Base64::standard();
    let error = codec.decode(b"???=").expect_err("alphabet");
    assert!(matches!(error, EncodingError::InvalidByte { offset: 0, byte: b'?' }));
}

#[test]
fn base64_reports_the_offending_offset() {
    let codec = Base64::standard();
    let error = codec.decode(b"AB?D").expect_err("alphabet");
    assert!(matches!(error, EncodingError::InvalidByte { offset: 2, byte: b'?' }));
}

#[test]
fn base64_flags_non_zero_padding_bits() {
    let codec = Base64::standard();
    let decoded = codec.decode(b"TR==").expect("decode");
    assert_eq!(decoded.value, b"M");
    assert_eq!(decoded.canonical, Canonicality::NonCanonical);
}

#[test]
fn base64_enforces_the_padding_profile() {
    let required = Base64::standard();
    assert!(matches!(required.decode(b"TW="), Err(EncodingError::InvalidPadding { .. })));
    assert!(matches!(required.decode(b"A"), Err(EncodingError::InvalidPadding { .. })));

    let unpadded = Base64::url_safe_no_padding();
    assert!(matches!(unpadded.decode(b"TQ=="), Err(EncodingError::InvalidPadding { .. })));
    assert_eq!(unpadded.decode(b"TQ").expect("unpadded").value, b"M");
}

#[test]
fn base64_exposes_the_slice_family() {
    fn round_trip<C: SliceCodec<Output = Vec<u8>>>(codec: &C, input: &[u8]) {
        let encoded = codec.encode(input);
        assert_eq!(codec.decode(&encoded).expect("decode").value, input);
    }

    round_trip(&Base64::standard(), b"data-encoding");
    round_trip(&Base64::url_safe_no_padding(), b"data-encoding");
}

#[test]
fn base64_whitespace_policy_is_explicit() {
    let rejecting = Base64::url_safe_no_padding();
    let error = rejecting.decode(b"TQ\n").expect_err("whitespace");
    assert!(matches!(error, EncodingError::InvalidByte { offset: 2, byte: b'\n' }));
}
#[test]
fn strict_protocol_profile_rejects_malleable_text() {
    use data_encoding::representation::Base64;

    let codec = Base64::standard_strict();
    assert_eq!(codec.encode_text(b"f"), "Zg==");
    assert_eq!(codec.decode_canonical("Zg==").unwrap(), b"f");
    assert!(codec.decode_canonical("Zh==").is_err());
    assert!(codec.decode_canonical(" Zg==").is_err());
    assert!(codec.decode_canonical("Zg").is_err());
}
