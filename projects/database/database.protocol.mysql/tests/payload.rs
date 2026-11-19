//! protocol.mysql 的 payload 原语合同：length-encoded、OK/EOF/ERR 与列定义。

use database_protocol_mysql::{
    LenencError, append_packet, column_definition_payload, encode_length_encoded, eof_payload, error_payload, ok_payload, prepare_ok_payload,
    read_length_encoded,
};

#[test]
fn length_encoded_uses_two_byte_form_for_251() {
    let mut bytes = Vec::new();
    encode_length_encoded(&mut bytes, 251);
    assert_eq!(bytes, vec![252, 251, 0]);
}

#[test]
fn length_encoded_round_trips_every_width() {
    for value in [0_u64, 1, 250, 251, 252, 65_535, 65_536, 16_777_215, 16_777_216, 4_294_967_295, 4_294_967_296, u64::MAX] {
        let mut bytes = Vec::new();
        encode_length_encoded(&mut bytes, value);
        let mut cursor = 0;
        assert_eq!(read_length_encoded(&bytes, &mut cursor), Ok(Some(value)), "value {value}");
        assert_eq!(cursor, bytes.len(), "value {value} consumed every byte");
    }
}

#[test]
fn length_encoded_reports_null_marker_and_errors() {
    assert_eq!(read_length_encoded(&[0xfb], &mut 0), Ok(None));
    assert_eq!(read_length_encoded(&[0xff], &mut 0), Err(LenencError::InvalidMarker));
    assert_eq!(read_length_encoded(&[], &mut 0), Err(LenencError::Truncated));
    assert_eq!(read_length_encoded(&[0xfc, 0x01], &mut 0), Err(LenencError::Truncated));
    assert_eq!(read_length_encoded(&[0xfe, 1, 2, 3], &mut 0), Err(LenencError::Truncated));
}

#[test]
fn ok_payload_keeps_classic_layout() {
    assert_eq!(ok_payload(0, 0, 2), vec![0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00]);
    assert_eq!(ok_payload(1, 7, 2), vec![0x00, 0x01, 0x07, 0x02, 0x00, 0x00, 0x00]);
}

#[test]
fn eof_payload_carries_status_flags() {
    assert_eq!(eof_payload(0x0001 | 0x0002), vec![0xFE, 0x00, 0x00, 0x03, 0x00]);
}

#[test]
fn error_payload_carries_code_state_and_message() {
    assert_eq!(
        error_payload(1064, "42000", "syntax"),
        vec![0xff, 0x28, 0x04, b'#', b'4', b'2', b'0', b'0', b'0', b's', b'y', b'n', b't', b'a', b'x']
    );
}

#[test]
fn prepare_ok_payload_orders_counts_after_id() {
    assert_eq!(prepare_ok_payload(1, 2, 3), vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn column_definition_declares_tiny_and_blob() {
    let tiny = column_definition_payload("flag", 0x01, false, 65_535);
    // 前缀：lenenc(3)+"def"、三个空 lenenc、lenenc(4)+"flag"、一个空 lenenc，共 13 字节。
    assert_eq!(&tiny[..13], &[0x03, b'd', b'e', b'f', 0x00, 0x00, 0x00, 0x04, b'f', b'l', b'a', b'g', 0x00]);
    // 固定段 13 字节：0x0C + charset(2) + column_length(4) + type(1) + flags(2) + decimals(1) + filler(2)。
    assert_eq!(&tiny[13..], &[0x0C, 0x0C, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00]);
    let blob = column_definition_payload("payload", 0xFC, true, 65_535);
    assert_eq!(&blob[..1], &[0x03]);
    let fixed = blob.len() - 13;
    assert_eq!(blob[fixed], 0x0C);
    assert_eq!(&blob[fixed + 1..fixed + 3], &[0x3F, 0x00]); // binary charset 63
    assert_eq!(&blob[fixed + 3..fixed + 7], &[0xFF, 0xFF, 0x00, 0x00]); // blob capacity
    assert_eq!(blob[fixed + 7], 0xFC);
    assert_eq!(&blob[fixed + 8..fixed + 10], &[0x00, 0x00]); // nullable → flags 0
}

#[test]
fn append_packet_frames_like_encode() {
    let mut out = vec![0xAA];
    append_packet(&mut out, 3, b"hi").unwrap();
    assert_eq!(out, vec![0xAA, 0x02, 0x00, 0x00, 0x03, b'h', b'i']);
}
