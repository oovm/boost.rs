//! Wire-format smoke tests: canonical encodings, round-trips through this
//! crate's own writer, and the failure modes a hostile or truncated input can
//! reach. Bytes are built in code — no binary fixtures.

use data_binary_protobuf::*;

/// Bytes a base-128 varint takes.
fn varint_width(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// The example from the protobuf encoding spec: field 1, varint 150.
#[test]
fn canonical_varint_field() {
    let mut reader = Reader::new(&[0x08, 0x96, 0x01]);
    let tag = reader.next_tag().unwrap().unwrap();
    assert_eq!(tag, Tag { number: 1, wire: WireType::Varint });
    assert_eq!(reader.read_u64().unwrap(), 150);
    assert!(reader.is_empty());
    assert_eq!(reader.next_tag().unwrap(), None);
}

#[test]
fn canonical_string_field() {
    let mut reader = Reader::new(&[0x12, 0x04, b't', b'e', b's', b't']);
    let tag = reader.next_tag().unwrap().unwrap();
    assert_eq!(tag, Tag { number: 2, wire: WireType::LengthDelimited });
    assert_eq!(reader.read_string(tag.number).unwrap(), "test");
}

#[test]
fn fixed_width_fields_are_little_endian() {
    let mut reader = Reader::new(&[0x0d, 0x01, 0x02, 0x03, 0x04, 0x09, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    let f32_tag = reader.next_tag().unwrap().unwrap();
    assert_eq!(f32_tag.wire, WireType::Fixed32);
    assert_eq!(reader.read_fixed32().unwrap(), 0x0403_0201);
    let f64_tag = reader.next_tag().unwrap().unwrap();
    assert_eq!(f64_tag.wire, WireType::Fixed64);
    assert_eq!(reader.read_fixed64().unwrap(), 0x0807_0605_0403_0201);
}

#[test]
fn zigzag_matches_the_spec_table() {
    assert_eq!(zigzag_encode_i32(0), 0);
    assert_eq!(zigzag_encode_i32(-1), 1);
    assert_eq!(zigzag_encode_i32(1), 2);
    assert_eq!(zigzag_encode_i32(-2), 3);
    assert_eq!(zigzag_encode_i32(i32::MIN), 0xffff_ffff);
    assert_eq!(zigzag_encode_i64(i64::MIN), 0xffff_ffff_ffff_ffff);
    for v in [0i32, 1, -1, 2, -2, 63, -64, i32::MIN, i32::MAX] {
        assert_eq!(zigzag_decode_i32(zigzag_encode_i32(v)), v);
    }
    for v in [0i64, 1, -1, i64::MIN, i64::MAX] {
        assert_eq!(zigzag_decode_i64(zigzag_encode_i64(v)), v);
    }
}

/// Every scalar writer must produce something the reader takes back unchanged.
#[test]
fn scalar_round_trip() {
    let mut w = Writer::new();
    w.write_u32(1, 4_294_967_295).unwrap();
    w.write_u64(2, u64::MAX).unwrap();
    w.write_i32(3, -2_147_483_648).unwrap();
    w.write_i64(4, i64::MIN).unwrap();
    w.write_sint32(5, -123_456).unwrap();
    w.write_sint64(6, -123_456_789_012_345).unwrap();
    w.write_bool(7, true).unwrap();
    w.write_enum(8, -3).unwrap();
    w.write_fixed32_field(9, 0xdead_beef).unwrap();
    w.write_fixed64_field(10, 0x0102_0304_0506_0708).unwrap();
    w.write_float(11, 1.5).unwrap();
    w.write_double(12, -2.25).unwrap();
    w.write_string(13, "onnx").unwrap();
    w.write_bytes(14, &[0x00, 0xff, 0x10]).unwrap();

    let bytes = w.into_vec();
    let mut r = Reader::new(&bytes);
    let mut seen = Vec::new();
    while let Some(tag) = r.next_tag().unwrap() {
        let value = match tag.wire {
            WireType::Varint => format!("v{}", r.read_varint().unwrap()),
            WireType::Fixed32 => format!("f32{}", r.read_fixed32().unwrap()),
            WireType::Fixed64 => format!("f64{}", r.read_fixed64().unwrap()),
            WireType::LengthDelimited => format!("b{}", r.read_bytes(tag.number).unwrap().len()),
            WireType::Group(_) => unreachable!(),
        };
        seen.push((tag.number, value));
    }
    assert_eq!(seen.first().unwrap().0, 1);
    assert_eq!(seen.len(), 14);
    assert_eq!(seen[0].1, format!("v{}", u64::from(u32::MAX)));
    assert_eq!(seen[4].1, "v246911"); // zigzag(-123456)
    assert_eq!(seen[12].1, "b4");
}

/// Negative `int32`/`int64` occupy the sign-extended 64-bit varint, which the
/// reader must accept and narrow back.
#[test]
fn negative_varints_survive_the_sign_extension() {
    for v in [i32::MIN, -1, 0, 1, i32::MAX] {
        let mut w = Writer::new();
        w.write_i32(1, v).unwrap();
        let bytes = w.into_vec();
        assert_eq!(bytes.len(), if v < 0 { 11 } else { 1 + varint_width(v as u64) }, "encoding width for {v}");
        let mut r = Reader::new(&bytes);
        r.next_tag().unwrap().unwrap();
        assert_eq!(r.read_i32().unwrap(), v);
    }
}

#[test]
fn nested_message_is_bounded_to_its_own_bytes() {
    let mut inner = Writer::new();
    inner.write_u32(1, 7).unwrap();
    let mut outer = Writer::new();
    outer.write_message(3, inner.as_slice()).unwrap();
    outer.write_u32(4, 9).unwrap();

    let bytes = outer.into_vec();
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert_eq!(tag.number, 3);
    let mut sub = r.read_message(tag.number).unwrap();
    assert_eq!(sub.depth(), 1);
    let inner_tag = sub.next_tag().unwrap().unwrap();
    assert_eq!(sub.read_u32().unwrap(), 7);
    assert_eq!(inner_tag.number, 1);
    assert!(sub.next_tag().unwrap().is_none());
    // Trailing bytes of the outer message are still reachable.
    let after = r.next_tag().unwrap().unwrap();
    assert_eq!(after.number, 4);
    assert_eq!(r.read_u32().unwrap(), 9);
}

#[test]
fn packed_repeated_round_trip() {
    let dims = [1u32, 3, 224, 224];
    let mut w = Writer::new();
    w.write_packed(1, &dims).unwrap();
    let bytes = w.into_vec();
    // tag(1, LEN)=0x0a, body of six bytes: 224 encodes as 0xe0 0x01.
    assert_eq!(bytes, vec![0x0a, 0x06, 0x01, 0x03, 0xe0, 0x01, 0xe0, 0x01]);

    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    let got: Vec<u32> = r.read_packed::<u32>(tag).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got, dims);
}

#[test]
fn packed_floats_keep_exact_bits() {
    let values = [0.5f32, -1.25, f32::MAX, f32::from_bits(0x7fc0_0000)];
    let mut w = Writer::new();
    w.write_packed(1, &values).unwrap();
    let bytes = w.into_vec();
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    let got: Vec<f32> = r.read_packed::<f32>(tag).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got[0], 0.5);
    assert_eq!(got[1], -1.25);
    assert_eq!(got[2], f32::MAX);
    assert!(got[3].is_nan());
}

/// A packed field whose declared body length is not a whole number of fixed
/// elements must fail rather than yield a short read.
#[test]
fn packed_truncation_is_reported_on_the_element() {
    let bytes = [0x0a, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00]; // 5 bytes of fixed32
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    let mut packed = r.read_packed::<f32>(tag).unwrap();
    assert!(packed.next().unwrap().is_ok());
    match packed.next() {
        Some(Err(ProtoError::Truncated { .. })) => {}
        other => panic!("expected truncation, got {other:?}"),
    }
}

#[test]
fn unknown_fields_are_skippable_without_a_schema() {
    let mut w = Writer::new();
    w.write_u32(1, 11).unwrap();
    w.write_string(99, "not in my schema").unwrap();
    w.write_fixed64_field(98, 0).unwrap();
    w.write_fixed32_field(97, 0).unwrap();
    w.write_bytes(96, &[1, 2, 3]).unwrap();
    w.write_u32(2, 22).unwrap();
    let bytes = w.into_vec();

    let mut r = Reader::new(&bytes);
    let mut known = Vec::new();
    while let Some(tag) = r.next_tag().unwrap() {
        if matches!(tag.number, 1 | 2) {
            known.push(r.read_u32().unwrap());
        } else {
            r.skip(tag).unwrap();
        }
    }
    assert_eq!(known, vec![11, 22]);
}

#[test]
fn generic_traversal_captures_every_wire_shape() {
    let mut inner = Writer::new();
    inner.write_u32(1, 5).unwrap();
    let mut w = Writer::new();
    w.write_u32(1, 7).unwrap();
    w.write_double(2, 1.0).unwrap();
    w.write_fixed32_field(3, 9).unwrap();
    w.write_message(4, inner.as_slice()).unwrap();

    let bytes = w.into_vec();
    let fields = Reader::new(&bytes).read_all().unwrap();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0], Field { number: 1, value: Value::Varint(7) });
    assert!(matches!(fields[1], Field { number: 2, value: Value::Fixed64(_) }));
    assert_eq!(fields[2], Field { number: 3, value: Value::Fixed32(9) });
    match &fields[3].value {
        Value::Bytes(b) => assert_eq!(Reader::new(b).read_all().unwrap().len(), 1),
        other => panic!("expected bytes, got {other:?}"),
    }
}

/// Groups are deprecated but must not be mistaken for corruption.
#[test]
fn group_round_trip_and_skip() {
    let mut w = Writer::new();
    w.write_tag(20, WireType::Group(20)).unwrap();
    w.write_u32(1, 3).unwrap();
    w.write_end_group(20).unwrap();
    w.write_u32(21, 4).unwrap();
    let bytes = w.into_vec();

    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert_eq!(tag.wire, WireType::Group(20));
    match r.read_value(tag).unwrap() {
        Value::Group(fields) => assert_eq!(fields, vec![Field { number: 1, value: Value::Varint(3) }]),
        other => panic!("expected group, got {other:?}"),
    }
    let after = r.next_tag().unwrap().unwrap();
    assert_eq!(after.number, 21);

    // Skipping a group consumes exactly its body.
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    r.skip(tag).unwrap();
    assert_eq!(r.next_tag().unwrap().unwrap().number, 21);
}

// --------------------------------------------------------------- failures ---

#[test]
fn empty_input_yields_no_fields() {
    let mut r = Reader::new(&[]);
    assert_eq!(r.next_tag().unwrap(), None);
    assert_eq!(r.read_all().unwrap(), Vec::new());
}

#[test]
fn unterminated_varint_is_rejected() {
    // Field 1, then ten continuation bytes that never terminate.
    let bytes = [0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80];
    let mut r = Reader::new(&bytes);
    r.next_tag().unwrap().unwrap();
    match r.read_varint() {
        Err(ProtoError::VarintTooLong { max, .. }) => assert_eq!(max, MAX_VARINT_BYTES),
        other => panic!("expected too-long varint, got {other:?}"),
    }
}

#[test]
fn overflowing_varint_is_rejected() {
    // Ten bytes whose last one carries a value bit beyond bit 63.
    let bytes = [0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x03];
    let mut r = Reader::new(&bytes);
    r.next_tag().unwrap().unwrap();
    assert!(matches!(r.read_varint(), Err(ProtoError::VarintOverflow { bits: 64, .. })));
}

/// A ten-byte varint whose last byte is `0x01` is the widest legal encoding.
#[test]
fn widest_legal_varint_is_accepted() {
    let bytes = [0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
    let mut r = Reader::new(&bytes);
    r.next_tag().unwrap().unwrap();
    assert_eq!(r.read_varint().unwrap(), u64::MAX);
}

#[test]
fn truncated_values_are_rejected() {
    let cases: &[&[u8]] = &[
        &[0x08],                                     // varint with no payload
        &[0x0d, 0x01, 0x02],                         // fixed32 missing two bytes
        &[0x09, 0x01],                               // fixed64 missing six bytes
        &[0x0a, 0x05, 0x01, 0x02],                   // length-delimited runs past the end
    ];
    for bytes in cases {
        let mut r = Reader::new(bytes);
        let tag = r.next_tag().unwrap();
        let result = match tag.unwrap().wire {
            WireType::Varint => r.read_varint().map(|_| ()),
            WireType::Fixed32 => r.read_fixed32().map(|_| ()),
            WireType::Fixed64 => r.read_fixed64().map(|_| ()),
            _ => r.read_bytes(1).map(|_| ()),
        };
        assert!(
            matches!(result, Err(ProtoError::Truncated { .. } | ProtoError::LengthBeyondEnd { .. })),
            "{bytes:x?} accepted"
        );
    }
}

#[test]
fn declared_length_beyond_the_buffer_is_rejected_before_allocating() {
    // A four-gigabyte claim inside a six-byte input.
    let bytes = [0x0a, 0x80, 0x80, 0x80, 0x80, 0x10];
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    match r.read_bytes(tag.number) {
        Err(ProtoError::LengthBeyondEnd { declared, available, .. }) => {
            assert_eq!(declared, 4_294_967_296);
            assert_eq!(available, 0);
        }
        other => panic!("expected beyond-end, got {other:?}"),
    }
}

#[test]
fn invalid_field_numbers_are_rejected() {
    for number in [0u32, 19_000, 19_999, MAX_FIELD_NUMBER + 1] {
        let mut w = Writer::new();
        assert!(
            matches!(w.write_u32(number, 1), Err(ProtoError::InvalidFieldNumber(n)) if n == number),
            "field {number} accepted"
        );
    }
    assert!(Writer::new().write_u32(MAX_FIELD_NUMBER, 1).is_ok());
    assert!(Writer::new().write_u32(RESERVED_FIELD_TO + 1, 1).is_ok());
}

#[test]
fn undefined_wire_type_is_rejected() {
    let bytes = [0x0c, 0x01]; // field 1, wire type 4 outside any group
    let mut r = Reader::new(&bytes);
    assert!(matches!(r.next_tag(), Err(ProtoError::StrayEndGroup(1))));

    let bytes = [0x0e, 0x01]; // field 1, wire type 6
    let mut r = Reader::new(&bytes);
    assert!(matches!(r.next_tag(), Err(ProtoError::UnknownWireType { field: 1, raw: 6 })));
}

#[test]
fn nesting_beyond_the_cap_is_rejected() {
    // Wrap an empty message in field 1 five times, then allow only four levels.
    let cap = 4usize;
    let mut body = Vec::new();
    for _ in 0..=cap {
        let mut w = Writer::new();
        w.write_message(1, &body).unwrap();
        body = w.into_vec();
    }

    // The same input is readable when the cap is generous.
    let mut ok = Reader::with_max_depth(&body, cap + 1);
    for _ in 0..=cap {
        let tag = ok.next_tag().unwrap().unwrap();
        ok = ok.read_message(tag.number).unwrap();
    }

    let mut r = Reader::with_max_depth(&body, cap);
    let mut result = Ok(());
    for _ in 0..=cap {
        let tag = r.next_tag().unwrap().unwrap();
        match r.read_message(tag.number) {
            Ok(sub) => r = sub,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    assert!(matches!(result, Err(ProtoError::TooDeep(d)) if d == cap), "got {result:?}");
}

#[test]
fn string_fields_validate_utf8() {
    let bytes = [0x0a, 0x02, 0xff, 0xfe];
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert_eq!(r.read_bytes(tag.number).unwrap(), &[0xff, 0xfe]);

    let bytes = [0x0a, 0x02, 0xff, 0xfe];
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert!(matches!(r.read_string(tag.number), Err(ProtoError::InvalidUtf8(1))));
}

#[test]
fn mismatched_group_terminator_is_rejected() {
    let mut w = Writer::new();
    w.write_tag(20, WireType::Group(20)).unwrap();
    w.write_end_group(21).unwrap();
    let bytes = w.into_vec();
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert!(matches!(r.skip(tag), Err(ProtoError::GroupMismatch { open: 20, close: 21 })));
}

#[test]
fn unterminated_group_is_rejected() {
    let mut w = Writer::new();
    w.write_tag(20, WireType::Group(20)).unwrap();
    w.write_u32(1, 3).unwrap();
    let bytes = w.into_vec();
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    assert!(matches!(r.skip(tag), Err(ProtoError::GroupUnterminated(20))));
}

#[test]
fn packed_refuses_a_wire_type_that_cannot_pack() {
    let mut w = Writer::new();
    w.write_string(1, "abc").unwrap();
    let bytes = w.into_vec();
    let mut r = Reader::new(&bytes);
    let tag = r.next_tag().unwrap().unwrap();
    // Reading a length-delimited field as packed bytes is not a thing.
    assert!(matches!(r.read_packed::<u32>(Tag { number: 1, wire: WireType::Varint }), Err(ProtoError::UnpackableWireType { .. })));
    assert!(r.read_packed::<u32>(tag).is_ok());
}

/// Determinism: encoding the same logical message twice is byte-identical,
/// which is what lets a cache key be derived from the bytes.
#[test]
fn encoding_is_deterministic() {
    let build = || {
        let mut w = Writer::new();
        w.write_string(1, "ResNet18").unwrap();
        w.write_packed(2, &[1u32, 3, 224, 224]).unwrap();
        w.write_float(3, 0.5).unwrap();
        w.into_vec()
    };
    assert_eq!(build(), build());
    assert!(!build().is_empty());
}
