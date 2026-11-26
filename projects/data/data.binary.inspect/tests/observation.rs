//! Integration coverage for the pure byte-observation modules.

use data_binary_inspect::{
    SearchMode,
    diff::{DiffOptions, diff},
    dump::hexdump,
    entropy::{range, series, shannon},
    find::{FindOptions, find},
    text::{TextMode, TextOptions, extract},
};

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[test]
fn diff_reports_equal_inputs() {
    let report = diff(b"abcdef", b"abcdef", &DiffOptions::default());
    assert_eq!(report.summary.equal_prefix, 6);
    assert_eq!(report.summary.equal_suffix, 0);
    assert_eq!(report.summary.changed_bytes, 0);
    assert!(report.ranges.is_empty());
    assert_eq!(report.total, 0);
    assert!(!report.truncated);
}

#[test]
fn diff_reports_a_single_changed_byte() {
    let report = diff(b"hello world", b"hello World", &DiffOptions::default());
    assert_eq!(report.summary.equal_prefix, 6);
    assert_eq!(report.summary.equal_suffix, 4);
    assert_eq!(report.summary.changed_bytes, 1);
    assert_eq!(report.total, 1);
    let range = &report.ranges[0];
    assert_eq!(range.offset, 6);
    assert_eq!(range.left_length, 1);
    assert_eq!(range.right_length, 1);
    assert_eq!(range.left_preview, "w");
    assert_eq!(range.right_preview, "W");
}

#[test]
fn diff_reports_an_insertion_as_a_narrow_right_range() {
    let report = diff(b"abc", b"abXc", &DiffOptions::default());
    assert_eq!(report.summary.equal_prefix, 2);
    assert_eq!(report.summary.equal_suffix, 1);
    assert_eq!(report.total, 1);
    let range = &report.ranges[0];
    assert_eq!(range.offset, 2);
    assert_eq!(range.left_length, 0);
    assert_eq!(range.right_length, 1);
    assert_eq!(range.left_preview, "");
    assert_eq!(range.right_preview, "X");
}

#[test]
fn diff_handles_empty_inputs() {
    let report = diff(b"", b"", &DiffOptions::default());
    assert_eq!(report.summary.changed_bytes, 0);
    assert!(report.ranges.is_empty());

    let added = diff(b"", b"abc", &DiffOptions::default());
    assert_eq!(added.summary.equal_prefix, 0);
    assert_eq!(added.summary.changed_bytes, 3);
    assert_eq!(added.total, 1);
    assert_eq!(added.ranges[0].left_length, 0);
    assert_eq!(added.ranges[0].right_length, 3);
}

#[test]
fn diff_truncates_ranges_at_the_budget() {
    let options = DiffOptions::new(1, 8);
    let report = diff(b"aXbXc", b"aYbYc", &options);
    assert_eq!(report.total, 2);
    assert_eq!(report.ranges.len(), 1);
    assert!(report.truncated);
    assert_eq!(report.summary.changed_bytes, 2);
    assert_eq!(report.ranges[0].offset, 1);
}

#[test]
fn find_returns_offsets_for_hex_ascii_and_utf16() {
    let buffer = b"\x00Hello\x00hello\x00".to_vec();

    let hex = find(&buffer, "48 65", &FindOptions::new(SearchMode::Hex)).unwrap();
    assert_eq!(hex.mode, "hex");
    assert_eq!(hex.hits[0].offset, 1);
    assert_eq!(hex.hits[0].length, 2);
    assert_eq!(hex.hits[0].preview, "48 65");

    let ascii = find(&buffer, "hello", &FindOptions::new(SearchMode::Ascii)).unwrap();
    assert_eq!(ascii.total, 2);
    assert_eq!(ascii.hits[0].offset, 1);

    let wide_data = [b"\x00\x00".as_slice(), &utf16("Hello")].concat();
    let wide = find(&wide_data, "Hello", &FindOptions::new(SearchMode::Utf16Le)).unwrap();
    assert_eq!(wide.total, 1);
    assert_eq!(wide.hits[0].offset, 2);
    assert_eq!(wide.hits[0].length, 10);
}

#[test]
fn find_enforces_the_hit_and_span_budgets() {
    let buffer = b"\x00Hello\x00Hello".to_vec();
    let options = FindOptions { max_hits: 1, ..FindOptions::new(SearchMode::Ascii) };
    let report = find(&buffer, "Hello", &options).unwrap();
    assert_eq!(report.total, 2);
    assert_eq!(report.hits.len(), 1);
    assert!(report.truncated);
    assert!(!report.span_limited);

    let span = FindOptions { max_span: Some(6), ..FindOptions::new(SearchMode::Ascii) };
    let limited = find(&buffer, "Hello", &span).unwrap();
    assert_eq!(limited.scanned, 6);
    assert_eq!(limited.total, 1);
    assert!(limited.span_limited);
}

#[test]
fn find_rejects_an_invalid_pattern() {
    let report = find(b"abc", "zz", &FindOptions::new(SearchMode::Hex));
    assert_eq!(report.unwrap_err().code(), "binary/parse");
}

#[test]
fn hexdump_keeps_alignment_at_a_non_zero_offset() {
    let buffer: Vec<u8> = (0u8..32).collect();
    let dump = hexdump(&buffer, 4, Some(16)).unwrap();
    assert!(dump.starts_with("00000004  04 05 06 07 08 "));
    assert_eq!(dump.lines().count(), 1);
}

#[test]
fn hexdump_truncates_the_final_row() {
    let buffer: Vec<u8> = (0u8..20).collect();
    let dump = hexdump(&buffer, 0, Some(20)).unwrap();
    assert_eq!(dump.lines().count(), 2);
    assert!(dump.contains("00000010  "));
    assert!(dump.ends_with("|....|\n"));
}

#[test]
fn hexdump_rejects_a_window_outside_the_buffer() {
    let buffer: Vec<u8> = (0u8..32).collect();
    assert_eq!(hexdump(&buffer, 30, Some(10)).unwrap_err().code(), "binary/range");
    assert_eq!(hexdump(&buffer, 0, Some(0)).unwrap(), "");
}

#[test]
fn entropy_separates_constant_from_uniform_data() {
    let constant = [0x41u8; 4096];
    assert_eq!(shannon(&constant), 0.0);

    let uniform: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    assert!((shannon(&uniform) - 8.0).abs() < 1e-9);

    assert_eq!(shannon(b""), 0.0);
    assert_eq!(range(&constant, 0, Some(16)).unwrap(), 0.0);
    assert_eq!(range(&constant, 0, Some(8192)).unwrap_err().code(), "binary/range");
}

#[test]
fn entropy_series_honours_window_step_and_limit() {
    let buffer = vec![0u8; 64];
    let report = series(&buffer, 0, None, 16, 16, 10).unwrap();
    assert_eq!(report.window, 16);
    assert_eq!(report.step, 16);
    assert_eq!(report.total, 4);
    assert!(!report.truncated);
    assert_eq!(report.windows[2].offset, 32);
    assert_eq!(report.windows[0].length, 16);

    let bounded = series(&buffer, 0, None, 16, 16, 2).unwrap();
    assert_eq!(bounded.total, 4);
    assert_eq!(bounded.windows.len(), 2);
    assert!(bounded.truncated);

    assert_eq!(series(&buffer, 0, None, 0, 16, 4).unwrap_err().code(), "binary/range");
    assert_eq!(series(&buffer, 0, None, 16, 0, 4).unwrap_err().code(), "binary/range");
}

#[test]
fn text_extracts_ascii_runs_with_min_length_and_budget() {
    let buffer = b"\x00alpha\x00beta\x00".to_vec();
    let report = extract(&buffer, &TextOptions::new(TextMode::Ascii, 3)).unwrap();
    assert_eq!(report.total, 2);
    assert_eq!(report.hits[0].offset, 1);
    assert_eq!(report.hits[0].length, 5);
    assert_eq!(report.hits[0].value, "alpha");
    assert_eq!(report.hits[1].offset, 7);

    let strict = extract(&buffer, &TextOptions::new(TextMode::Ascii, 5)).unwrap();
    assert_eq!(strict.total, 1);

    let bounded = extract(&buffer, &TextOptions { limit: Some(1), ..TextOptions::new(TextMode::Ascii, 3) }).unwrap();
    assert_eq!(bounded.total, 2);
    assert_eq!(bounded.hits.len(), 1);
    assert!(bounded.truncated);
}

#[test]
fn text_extracts_utf16le_runs() {
    let buffer = [&[0u8, 0u8][..], &utf16("Hi"), &[0u8, 0u8], &utf16("World")].concat();
    let report = extract(&buffer, &TextOptions::new(TextMode::Utf16Le, 2)).unwrap();
    assert_eq!(report.total, 2);
    assert_eq!(report.hits[0].offset, 2);
    assert_eq!(report.hits[0].length, 4);
    assert_eq!(report.hits[0].value, "Hi");
    assert_eq!(report.hits[1].offset, 8);
    assert_eq!(report.hits[1].value, "World");
}

#[test]
fn text_window_offsets_are_absolute() {
    let buffer = b"xxalpha".to_vec();
    let options = TextOptions { offset: 2, length: Some(5), ..TextOptions::new(TextMode::Ascii, 3) };
    let report = extract(&buffer, &options).unwrap();
    assert_eq!(report.total, 1);
    assert_eq!(report.hits[0].offset, 2);
    assert_eq!(report.hits[0].value, "alpha");

    let outside = TextOptions { offset: 4, length: Some(16), ..TextOptions::new(TextMode::Ascii, 3) };
    assert_eq!(extract(&buffer, &outside).unwrap_err().code(), "binary/range");
}
