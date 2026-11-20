use core::range::Range;

use data_text_core::{
    Arc,
    source::Source,
    vfs::{DocumentSession, MemoryVfs},
};

#[test]
fn document_session_bumps_revision_on_change() {
    let vfs = MemoryVfs::new();
    let first = vfs.open_document("file://a", Arc::<str>::from("a"));
    let second = vfs.change_document("file://a", Arc::<str>::from("ab"));
    assert_ne!(first.revision(), second.revision());
    assert_eq!(second.source().get_text_in(Range { start: 0, end: 2 }), "ab");
}

#[test]
fn request_source_prefers_open_buffer() {
    let vfs = MemoryVfs::new();
    vfs.write_file("file://a", Arc::<str>::from("disk"));
    vfs.open_document("file://a", Arc::<str>::from("buffer"));
    assert_eq!(vfs.request_source("file://a").unwrap().get_text_in(Range { start: 0, end: 6 }), "buffer");
    assert!(vfs.document_snapshot("file://a").is_some());
}

#[test]
fn close_document_clears_snapshot() {
    let vfs = MemoryVfs::new();
    vfs.open_document("file://a", Arc::<str>::from("open"));
    assert!(vfs.document_snapshot("file://a").is_some());
    vfs.close_document("file://a");
    assert!(vfs.document_snapshot("file://a").is_none());
}

#[test]
fn revisions_grow_numerically_and_stay_per_uri() {
    let vfs = MemoryVfs::new();
    let first = vfs.open_document("file://a", Arc::<str>::from("a"));
    let second = vfs.change_document("file://a", Arc::<str>::from("ab"));
    let other = vfs.open_document("file://b", Arc::<str>::from("b"));

    let number = |snapshot: &data_text_core::source::DocumentSnapshot| snapshot.revision().as_str().parse::<u64>().expect("numeric revision");
    assert!(number(&second) > number(&first));
    assert!(number(&other) > number(&second));
    assert_eq!(vfs.document_snapshot("file://a").unwrap().revision(), second.revision());
}

#[test]
fn stale_snapshot_keeps_the_text_it_was_taken_from() {
    let vfs = MemoryVfs::new();
    let stale = vfs.open_document("file://a", Arc::<str>::from("alpha"));
    vfs.change_document("file://a", Arc::<str>::from("beta"));

    assert_eq!(stale.source().get_text_in(Range { start: 0, end: 5 }), "alpha", "旧快照不得观察到后续编辑");
    assert_ne!(vfs.document_snapshot("file://a").unwrap().revision(), stale.revision());
    assert_eq!(vfs.request_source("file://a").unwrap().get_text_in(Range { start: 0, end: 4 }), "beta");
}
