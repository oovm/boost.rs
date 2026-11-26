//! `asar` container facts: the pickle header, the JSON tree and extraction.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use data_binary_container::asar::{AsarOptions, analyze};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("data-container-{}-{id}-{tag}", std::process::id()))
}

/// The JSON header of the synthetic asar fixture.
const ASAR_JSON: &str = concat!(
    r#"{"files":{"a.txt":{"size":"13","offset":"0"},"dir":{"files":{"b.txt":"#,
    r#"{"size":"4","offset":"13"}}},"loose.txt":{"size":"7","offset":"0","unpacked":true}}}"#
);

/// The packed data blob referenced by [`ASAR_JSON`].
const ASAR_DATA: &[u8] = b"alpha-payloadbeta";

/// Assembles a minimal asar archive: four header words, the JSON, the data blob.
fn asar_bytes() -> Vec<u8> {
    let json = ASAR_JSON.as_bytes();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&((8 + json.len()) as u32).to_le_bytes());
    bytes.extend_from_slice(&((json.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
    bytes.extend_from_slice(json);
    bytes.extend_from_slice(ASAR_DATA);
    bytes
}

#[test]
fn asar_lists_the_header_tree() {
    let bytes = asar_bytes();
    let report = analyze(&bytes, &AsarOptions::default()).expect("decode asar");

    let json_len = ASAR_JSON.len() as u64;
    assert_eq!(report.header.start_offset, 0);
    assert_eq!(report.header.data_start, 16 + json_len);
    assert_eq!(report.entry_count, 3);
    assert_eq!(report.directory_count, 1);
    assert_eq!(report.packed_bytes, 17);
    assert!(!report.truncated);

    let paths: Vec<&str> = report.entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, vec!["a.txt", "dir/b.txt", "loose.txt"]);
    assert_eq!(report.entries[0].offset, Some(16 + json_len));
    assert_eq!(report.entries[1].offset, Some(16 + json_len + 13));
    assert!(report.entries[2].unpacked);
    assert_eq!(report.entries[2].offset, None, "unpacked entries carry no packed bytes");

    let packed = &bytes[(16 + json_len) as usize..(16 + json_len + 13) as usize];
    assert_eq!(packed, b"alpha-payload".as_slice());

    let cut = analyze(&bytes, &AsarOptions { entries_limit: 1, ..AsarOptions::default() }).expect("cut listing");
    assert_eq!(cut.entries.len(), 1);
    assert!(cut.truncated);
}

#[test]
fn asar_extracts_packed_entries() {
    let bytes = asar_bytes();
    let dir = temp_dir("asar");

    let report = analyze(&bytes, &AsarOptions { extract_dir: Some(dir.display().to_string()), ..AsarOptions::default() })
        .expect("decode with extraction");

    let written = report.entries[1].extracted.as_ref().expect("nested entry").replace('\\', "/");
    assert!(written.ends_with("/dir/b.txt"), "the archive path is mirrored: {written}");
    assert_eq!(fs::read(&written).expect("read entry"), b"beta");
    assert!(report.entries[2].extracted.is_none(), "unpacked entries are not written");
    fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn asar_rejects_a_non_asar_input() {
    let mut bytes = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    bytes.resize(64, 0);
    let error = analyze(&bytes, &AsarOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
    assert_eq!(error.span(), Some(data_binary_container::Span::new(0, 4)));
}
