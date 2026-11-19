use boost_source::{
    ByteRange, ByteSink, ByteSource, ByteWindow, ContentIdentity, MemorySink, MemorySource, RangeMapSource, ReadError, ReadOutcome,
    SinkCapabilities, SourceCapabilities, WriteError, WriteOutcome, content_digest_of,
};

#[test]
fn memory_source_reads_without_padding() {
    let source = MemorySource::from_static("sample", b"ABCDE");
    let mut buffer = [0u8; 3];
    let outcome = source.read_at(3, &mut buffer).expect("read succeeds");
    assert_eq!(outcome, ReadOutcome::Partial { bytes_read: 2 });
    assert_eq!(&buffer[..2], b"DE");
    assert_eq!(buffer[2], 0);
}

#[test]
fn memory_source_reports_eof_without_zero_fill() {
    let source = MemorySource::from_static("sample", b"AB");
    let mut buffer = [9u8; 4];
    let outcome = source.read_at(2, &mut buffer).expect("read succeeds");
    assert_eq!(outcome, ReadOutcome::Eof);
    assert_eq!(buffer, [9, 9, 9, 9]);
}

#[test]
fn partial_source_reports_unavailable_range() {
    let source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3])]).with_total_length(16);
    let mut buffer = [0u8; 4];
    let outcome = source.read_at(8, &mut buffer).expect("read succeeds");
    assert!(matches!(outcome, ReadOutcome::Unavailable { .. }));
}

#[test]
fn read_range_surfaces_short_read() {
    let source = MemorySource::from_static("sample", b"ABCDE");
    let range = ByteRange::with_len(0, 8).expect("range");
    let error = source.read_range(range).unwrap_err();
    assert!(matches!(error, ReadError::ShortRead { expected: 8, actual: 5 }));
}

#[test]
fn byte_window_maps_outer_offsets() {
    let source = MemorySource::from_static("sample", b"0123456789");
    let window = ByteWindow::new(source, 0, ByteRange::with_len(2, 4).expect("range"));
    let bytes = window.read_range(ByteRange::with_len(0, 4).expect("inner")).expect("read");
    assert_eq!(bytes, b"2345");
}

#[test]
fn content_digest_tracks_content_changes() {
    assert_eq!(content_digest_of(b"ABCDE"), content_digest_of(b"ABCDE"));
    assert_ne!(content_digest_of(b"ABCDE"), content_digest_of(b"ABCDF"));
}

#[test]
fn snapshot_reuse_requires_a_matching_digest() {
    let source = MemorySource::from_static("sample", b"ABCDE");
    let identity = source.identity();

    let same_bytes = MemorySource::from_static("sample", b"ABCDE");
    assert!(identity.snapshot_reuses(&same_bytes.identity()));

    let changed_bytes = MemorySource::from_static("sample", b"ABCDF");
    assert!(!identity.snapshot_reuses(&changed_bytes.identity()));

    let renamed = MemorySource::from_static("moved", b"ABCDE");
    assert!(!identity.snapshot_reuses(&renamed.identity()));

    let labelled_only = ContentIdentity::new("sample");
    assert!(!identity.snapshot_reuses(&labelled_only));
}

#[test]
fn partial_source_identity_cannot_prove_content() {
    let source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3])]).with_total_length(16);
    let materialized = MemorySource::from_static("remote", b"ABCDE");
    assert!(!source.identity().snapshot_reuses(&materialized.identity()));
    assert!(source.identity().content_digest.is_none());
}

#[test]
fn byte_window_identity_is_not_its_parent_identity() {
    let source = MemorySource::from_static("sample", b"0123456789");
    let parent = source.identity();
    let window = ByteWindow::new(source, 0, ByteRange::with_len(2, 4).expect("range"));
    let windowed = window.identity();

    assert_ne!(windowed.label, parent.label);
    assert!(!windowed.snapshot_reuses(&parent));
}

#[test]
fn supplied_range_satisfies_a_previous_need() {
    let mut source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3, 4])]).with_total_length(8);
    let mut buffer = [0u8; 4];
    let outcome = source.read_at(4, &mut buffer).expect("read succeeds");
    match outcome {
        ReadOutcome::Unavailable { range, .. } => {
            assert_eq!(range, ByteRange::with_len(4, 4).expect("range"));
        }
        other => panic!("expected a missing range, got {other:?}"),
    }

    source.provide_range(4, vec![5, 6, 7, 8]).expect("supply the fetched range");
    let outcome = source.read_at(4, &mut buffer).expect("read succeeds");
    assert_eq!(outcome, ReadOutcome::Complete);
    assert_eq!(buffer, [5, 6, 7, 8]);
}

#[test]
fn supplied_range_rejects_contradicting_bytes() {
    let mut source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3, 4])]).with_total_length(8);
    let error = source.provide_range(2, vec![9, 9]).expect_err("contradiction");
    assert!(matches!(error, ReadError::ConflictingRange { .. }));

    source.provide_range(2, vec![3, 4]).expect("identical overlap is accepted");
    let mut buffer = [0u8; 2];
    assert_eq!(source.read_at(2, &mut buffer).expect("read succeeds"), ReadOutcome::Complete);
    assert_eq!(buffer, [3, 4]);
}

#[test]
fn supplied_range_past_a_known_length_is_rejected() {
    let mut source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3])]).with_total_length(4);
    let error = source.provide_range(3, vec![4, 5]).expect_err("past the end");
    assert!(matches!(error, ReadError::Unavailable { .. }));
}

#[test]
fn reads_past_a_known_length_report_eof() {
    let source = RangeMapSource::new("remote", vec![(0, vec![1, 2, 3])]).with_total_length(4);
    let mut buffer = [0u8; 2];
    assert_eq!(source.read_at(4, &mut buffer).expect("read succeeds"), ReadOutcome::Eof);
}

#[test]
fn capability_presets_are_distinct() {
    assert_eq!(MemorySource::from_static("sample", b"A").capabilities(), SourceCapabilities::MEMORY);
    assert_eq!(RangeMapSource::new("remote", Vec::new()).capabilities(), SourceCapabilities::PARTIAL);
    assert_eq!(MemorySink::new().capabilities(), SinkCapabilities::MEMORY);
}

#[test]
fn memory_sink_appends_and_patches() {
    let mut sink = MemorySink::new();
    assert_eq!(sink.append(b"hello").expect("append"), WriteOutcome::Complete);
    sink.write_at(0, b"HELLO").expect("write_at");
    sink.patch_u32_le(5, 0x0102_0304).expect("patch");
    assert_eq!(sink.bytes(), b"HELLO\x04\x03\x02\x01");
}

#[test]
fn memory_sink_enforces_budget_on_append() {
    let mut sink = MemorySink::with_budget(4);
    assert_eq!(sink.append(b"abc").expect("append"), WriteOutcome::Complete);
    assert_eq!(sink.append(b"de").expect("append"), WriteOutcome::WouldExceedBudget { attempted: 5, limit: 4 });
}

#[test]
fn memory_sink_enforces_budget_on_seek_write() {
    let mut sink = MemorySink::with_budget(4);
    let error = sink.write_at(2, b"abcd").expect_err("budget");
    assert!(matches!(error, WriteError::BudgetExceeded { attempted: 6, limit: 4 }));
}
