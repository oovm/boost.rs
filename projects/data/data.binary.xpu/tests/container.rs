//! `.xpu` shell tests. Every container is assembled in code through the
//! writer — no checked-in binaries. Negative cases tamper the built bytes
//! and re-seal the directory digest where needed, so each check fires on
//! its own rule.

use data_binary_xpu::*;

fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

fn minimal_spec() -> ContainerSpec {
    ContainerSpec {
        sections: vec![
            SectionInput { kind: 1, schema_rev: 0, flags: 0, alignment: 0, data: payload(1, 64) },
            SectionInput { kind: 2, schema_rev: 0, flags: 0, alignment: 0, data: payload(2, 128) },
            SectionInput { kind: 3, schema_rev: 0, flags: 0, alignment: 0, data: payload(3, 32) },
            SectionInput { kind: 4, schema_rev: 0, flags: 0, alignment: 0, data: payload(4, 4096) },
        ],
    }
}

fn minimal() -> Vec<u8> {
    build(&minimal_spec()).unwrap()
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Absolute offset of directory entry `i` (0-based).
fn entry(i: usize) -> usize {
    HEADER_BYTES + i * ENTRY_BYTES
}

/// Recompute the header directory digest over the current directory bytes.
fn reseal(b: &mut [u8]) {
    let count = u32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let digest = sha256_bytes(&b[HEADER_BYTES..HEADER_BYTES + count * ENTRY_BYTES]);
    b[48..80].copy_from_slice(&digest);
}

// -------------------------------------------------------------- happy path ---

#[test]
fn round_trip_exposes_header_directory_and_payloads() {
    let bytes = minimal();
    let c = Container::parse(&bytes).unwrap();
    assert_eq!(c.header().file_bytes, bytes.len() as u64);
    assert_eq!(c.header().manifest_id, 1);
    assert_eq!(c.sections().iter().map(|s| s.id).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(c.manifest().kind_parsed(), SectionKind::Manifest);
    assert_eq!(c.read(1).unwrap(), payload(1, 64).as_slice());
    assert_eq!(c.read(4).unwrap(), payload(4, 4096).as_slice());
    assert!(c.read(9).is_none());
    assert_eq!(c.kind_sections(SectionKind::Weight).count(), 1);
}

#[test]
fn weights_land_on_64_byte_alignment_by_default() {
    let bytes = minimal();
    let c = Container::parse(&bytes).unwrap();
    let w = c.kind_sections(SectionKind::Weight).next().unwrap();
    assert_eq!(w.alignment, WEIGHT_ALIGNMENT);
    assert_eq!(w.offset % WEIGHT_ALIGNMENT as u64, 0);
}

#[test]
fn digests_verify_after_a_clean_build() {
    let bytes = minimal();
    let c = Container::parse(&bytes).unwrap();
    for id in 1..=4 {
        c.verify(id).unwrap();
    }
    c.verify_all().unwrap();
}

#[test]
fn build_is_deterministic() {
    assert_eq!(build(&minimal_spec()).unwrap(), minimal());
}

#[test]
fn empty_sections_round_trip() {
    let mut spec = minimal_spec();
    spec.sections.push(SectionInput { kind: 8, schema_rev: 0, flags: 0, alignment: 0, data: Vec::new() });
    let bytes = build(&spec).unwrap();
    let c = Container::parse(&bytes).unwrap();
    assert_eq!(c.read(5).unwrap(), &[] as &[u8]);
    c.verify(5).unwrap();
}

#[test]
fn unknown_optional_kind_is_carried_through() {
    let mut spec = minimal_spec();
    spec.sections.push(SectionInput { kind: 42, schema_rev: 7, flags: 0, alignment: 0, data: payload(9, 16) });
    let bytes = build(&spec).unwrap();
    let c = Container::parse(&bytes).unwrap();
    let s = c.section(5).unwrap();
    assert_eq!(s.kind_parsed(), SectionKind::Unknown(42));
    assert_eq!(s.schema_rev, 7);
    assert_eq!(c.read(5).unwrap(), payload(9, 16).as_slice());
}

// ------------------------------------------------------------- tampering ----

#[test]
fn tampered_directory_fails_the_directory_digest() {
    let mut bytes = minimal();
    bytes[entry(0)] ^= 1;
    assert!(matches!(Container::parse(&bytes), Err(XpuError::DirectoryDigestMismatch)));
}

#[test]
fn tampered_payload_fails_only_section_verification() {
    let mut bytes = minimal();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let c = Container::parse(&bytes).unwrap();
    assert!(matches!(c.verify(4), Err(XpuError::SectionDigestMismatch(4))));
    assert!(c.verify(1).is_ok());
    assert!(matches!(c.verify_all(), Err(XpuError::SectionDigestMismatch(4))));
}

#[test]
fn bad_magic_is_rejected() {
    let mut bytes = minimal();
    bytes[0] ^= 0xff;
    assert!(matches!(Container::parse(&bytes), Err(XpuError::BadMagic)));
}

#[test]
fn unknown_major_is_rejected() {
    let mut bytes = minimal();
    put_u16(&mut bytes, 8, 2);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::UnsupportedVersion { major: 2, .. })));
}

#[test]
fn wrong_entry_bytes_field_is_rejected() {
    let mut bytes = minimal();
    put_u16(&mut bytes, 14, 64);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::HeaderField(_))));
}

#[test]
fn moved_directory_offset_is_rejected() {
    let mut bytes = minimal();
    put_u64(&mut bytes, 24, 176);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::HeaderField(_))));
}

#[test]
fn nonzero_header_reserved_is_rejected() {
    let mut bytes = minimal();
    bytes[80] = 1;
    assert!(matches!(Container::parse(&bytes), Err(XpuError::ReservedNonZero)));
}

#[test]
fn undefined_required_flags_are_rejected() {
    let mut bytes = minimal();
    put_u32(&mut bytes, 16, 1);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::UnknownRequiredFlags(1))));
}

#[test]
fn truncated_file_fails_the_length_check() {
    let mut bytes = minimal();
    bytes.truncate(bytes.len() - 1);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::FileLengthMismatch { .. })));
}

#[test]
fn header_shorter_than_96_bytes_is_too_short() {
    let bytes = minimal();
    assert!(matches!(Container::parse(&bytes[..95]), Err(XpuError::TooShort { need: 96, have: 95 })));
}

#[test]
fn oversized_section_count_is_limited_before_allocation() {
    let mut bytes = minimal();
    put_u32(&mut bytes, 20, 65_537);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::TooManySections(65_537))));
}

#[test]
fn directory_must_be_sorted_by_increasing_nonzero_id() {
    let mut bytes = minimal();
    put_u32(&mut bytes, entry(1), 1);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::BadOrder)));

    let mut bytes = minimal();
    put_u32(&mut bytes, entry(0), 0);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::BadOrder)));
}

#[test]
fn reserved_section_flag_bits_are_rejected() {
    let mut bytes = minimal();
    put_u32(&mut bytes, entry(0) + 8, FLAG_GLOBAL_REQUIRED | 2);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::ReservedSectionFlags(_))));
}

#[test]
fn alignment_must_be_a_power_of_two_of_at_least_8() {
    let mut bytes = minimal();
    put_u32(&mut bytes, entry(0) + 12, 12);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::BadAlignment(1))));
}

#[test]
fn stored_and_logical_must_match_in_v1() {
    let mut bytes = minimal();
    put_u64(&mut bytes, entry(0) + 24, 32);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::Compressed(1))));
}

#[test]
fn control_budget_is_enforced_over_declared_sizes() {
    let mut bytes = minimal();
    let huge = MAX_CONTROL_BYTES + 1;
    put_u64(&mut bytes, entry(0) + 24, huge);
    put_u64(&mut bytes, entry(0) + 32, huge);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::ControlBudgetExceeded { .. })));
}

#[test]
fn sections_must_start_after_the_directory() {
    let mut bytes = minimal();
    put_u64(&mut bytes, entry(0) + 16, 96);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::RangeViolation { id: 1, .. })));
}

#[test]
fn sections_must_fit_inside_the_file() {
    let mut bytes = minimal();
    put_u64(&mut bytes, entry(3) + 24, 1 << 40);
    put_u64(&mut bytes, entry(3) + 32, 1 << 40);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::RangeViolation { id: 4, .. })));
}

#[test]
fn offset_must_honour_the_declared_alignment() {
    let mut bytes = minimal();
    let at = entry(1) + 16;
    let old = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    put_u64(&mut bytes, at, old + 4);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::Misaligned { id: 2, .. })));
}

#[test]
fn non_empty_sections_may_not_overlap() {
    let mut bytes = minimal();
    let at = entry(1) + 16;
    let old = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    put_u64(&mut bytes, at, old - 8);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::Overlap { .. })));
}

#[test]
fn missing_required_singleton_is_rejected() {
    let mut bytes = minimal();
    put_u16(&mut bytes, entry(1) + 4, 8);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::MissingSingleton(2))));
}

#[test]
fn duplicated_required_singleton_is_rejected() {
    let mut bytes = minimal();
    put_u16(&mut bytes, entry(3) + 4, 1);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::DuplicateSingleton { kind: 1, count: 2 })));
}

#[test]
fn globally_required_unknown_kind_is_rejected() {
    let mut bytes = minimal();
    put_u16(&mut bytes, entry(3) + 4, 42);
    put_u32(&mut bytes, entry(3) + 8, FLAG_GLOBAL_REQUIRED);
    reseal(&mut bytes);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::RequiredUnknownKind(4))));
}

#[test]
fn manifest_reference_must_point_at_the_manifest() {
    let mut bytes = minimal();
    put_u32(&mut bytes, 40, 999);
    assert!(matches!(Container::parse(&bytes), Err(XpuError::ManifestRefMismatch(999))));
}

// ------------------------------------------------------------ writer input ---

#[test]
fn writer_rejects_incomplete_specs() {
    let no_sections = ContainerSpec { sections: Vec::new() };
    assert!(matches!(build(&no_sections), Err(XpuError::WriterInput(_))));

    let mut missing_graph = minimal_spec();
    missing_graph.sections.retain(|s| s.kind != 2);
    assert!(matches!(build(&missing_graph), Err(XpuError::WriterInput(_))));

    let mut two_manifests = minimal_spec();
    two_manifests.sections[3].kind = 1;
    assert!(matches!(build(&two_manifests), Err(XpuError::WriterInput(_))));

    let mut unknown_required = minimal_spec();
    unknown_required.sections[3] = SectionInput { kind: 42, schema_rev: 0, flags: FLAG_GLOBAL_REQUIRED, alignment: 0, data: vec![0] };
    assert!(matches!(build(&unknown_required), Err(XpuError::WriterInput(_))));

    let mut reserved_flags = minimal_spec();
    reserved_flags.sections[0].flags = 4;
    assert!(matches!(build(&reserved_flags), Err(XpuError::WriterInput(_))));

    let mut bad_alignment = minimal_spec();
    bad_alignment.sections[0].alignment = 24;
    assert!(matches!(build(&bad_alignment), Err(XpuError::WriterInput(_))));
}

#[test]
fn writer_output_always_parses_back() {
    // Property-style smoke: several shapes all survive the round trip.
    for (weight_len, extra) in [(0usize, false), (1, false), (4096, true)] {
        let mut spec = minimal_spec();
        spec.sections[3].data = payload(4, weight_len);
        if extra {
            spec.sections.push(SectionInput {
                kind: SectionKind::Module.as_raw(),
                schema_rev: 1,
                flags: 0,
                alignment: 4096,
                data: payload(7, 100),
            });
        }
        let bytes = build(&spec).unwrap();
        let c = Container::parse(&bytes).unwrap();
        c.verify_all().unwrap();
    }
}
