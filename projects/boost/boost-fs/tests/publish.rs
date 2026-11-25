use std::io::{ErrorKind, Write};

use boost_fs::{
    AtomicPublisher, OverwritePolicy, PublishEntry, PublishOptions, publish_batch, publish_bytes, publish_bytes_with_options, read_to_string,
};

#[test]
fn publish_bytes_creates_destination_atomically() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");

    publish_bytes(&target, br#"{"ok":true}"#).expect("publish");

    assert_eq!(read_to_string(&target).expect("read"), r#"{"ok":true}"#);
    let entries: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("report.json")]);
}

#[test]
fn publish_refuses_existing_destination() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");
    publish_bytes(&target, b"v1").expect("first publish");

    let error = publish_bytes(&target, b"v2").expect_err("second publish");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(read_to_string(&target).expect("read"), "v1");
}

#[test]
fn publish_replace_overwrites_destination() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");
    publish_bytes(&target, b"v1").expect("first publish");

    publish_bytes_with_options(&target, b"v2", PublishOptions { overwrite: OverwritePolicy::Replace, fsync_file: false, fsync_parent: false })
        .expect("replace publish");

    assert_eq!(read_to_string(&target).expect("read"), "v2");
}

#[test]
fn atomic_publisher_drops_staging_file_on_abort() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");

    let publisher =
        AtomicPublisher::create(&target, PublishOptions { overwrite: OverwritePolicy::RefuseExisting, fsync_file: false, fsync_parent: false })
            .expect("create publisher");
    let staging_path = publisher.staging_path().to_path_buf();
    assert!(staging_path.exists());
    drop(publisher);
    assert!(!staging_path.exists());
    assert!(!target.exists());
}

/// A destination that appears while the file is staged is refused at publish time, without replacing
/// the destination and without leaving the staging file behind.
#[test]
fn refused_publish_keeps_the_destination_and_removes_the_staging_file() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");

    let mut publisher =
        AtomicPublisher::create(&target, PublishOptions { overwrite: OverwritePolicy::RefuseExisting, fsync_file: false, fsync_parent: false })
            .expect("create publisher");
    let staging_path = publisher.staging_path().to_path_buf();
    publisher.write_all(b"staged").expect("write staged bytes");

    std::fs::write(&target, b"external").expect("create the destination after staging");

    let error = publisher.commit().expect_err("the publish is refused");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(read_to_string(&target).expect("read"), "external");
    assert!(!staging_path.exists(), "the refused publish left its staging file behind");

    let entries: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("report.json")]);
}

/// Publishing by rename replaces the destination and closes the handle it staged, so a replaced file
/// is readable again immediately.
#[test]
fn replace_publish_leaves_no_staging_file() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("report.json");
    publish_bytes(&target, b"v1").expect("first publish");

    let mut publisher =
        AtomicPublisher::create(&target, PublishOptions { overwrite: OverwritePolicy::Replace, fsync_file: true, fsync_parent: true })
            .expect("create publisher");
    publisher.write_all(b"v2").expect("write staged bytes");
    let staging_path = publisher.staging_path().to_path_buf();
    let published = publisher.commit().expect("replace publish");

    assert_eq!(published, target);
    assert_eq!(read_to_string(&target).expect("read"), "v2");
    assert!(!staging_path.exists());

    let entries: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("report.json")]);
}

/// Two publishers racing for one refused destination produce exactly one file: the loser sees
/// `AlreadyExists` and the winner's bytes are the only content on disk.
#[test]
fn concurrent_refused_publishers_elect_one_winner() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let target = base.path().join("contested.json");
    let barrier = std::sync::Barrier::new(2);

    let publish = |tag: &'static str| {
        barrier.wait();
        let outcome = publish_bytes_with_options(&target, tag.as_bytes(), PublishOptions::default());
        (tag, outcome.map_err(|error| error.kind()))
    };

    let (first, second) = std::thread::scope(|scope| {
        (scope.spawn(|| publish("first")).join().unwrap(), scope.spawn(|| publish("second")).join().unwrap())
    });
    let outcomes = [first, second];
    let winners = outcomes.iter().filter(|(_, outcome)| outcome.is_ok()).count();
    assert_eq!(winners, 1, "exactly one publisher must win: {outcomes:?}");
    assert!(outcomes.iter().any(|(_, outcome)| *outcome == Err(ErrorKind::AlreadyExists)));
    let winner = outcomes.iter().find(|(_, outcome)| outcome.is_ok()).expect("one winner").0;
    assert_eq!(read_to_string(&target).expect("read"), *winner);

    let entries: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("contested.json")], "a losing publisher left a staging file behind");
}

/// A batch that names the same destination twice, or hits an existing destination under
/// `RefuseExisting`, is refused before any file is written.
#[test]
fn publish_batch_refuses_conflicts_before_writing() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let first = base.path().join("first.json");
    let second = base.path().join("second.json");

    let failure = publish_batch(
        &[PublishEntry { target: first.clone(), bytes: b"a" }, PublishEntry { target: first.clone(), bytes: b"b" }],
        PublishOptions::default(),
    )
    .expect_err("duplicate batch targets");
    assert_eq!(failure.error.kind(), ErrorKind::AlreadyExists);
    assert!(failure.published.is_empty());
    assert!(!first.exists());

    std::fs::write(&second, b"external").expect("create an existing destination");
    let failure = publish_batch(
        &[PublishEntry { target: first.clone(), bytes: b"a" }, PublishEntry { target: second.clone(), bytes: b"b" }],
        PublishOptions::default(),
    )
    .expect_err("existing destination");
    assert_eq!(failure.error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(failure.target, second);
    assert!(failure.published.is_empty(), "a refused pre-check must not publish earlier entries");
    assert!(!first.exists());
    assert_eq!(read_to_string(&second).expect("read"), "external");
}

/// A failure part-way through a batch stops the remaining entries and reports the destinations
/// that were already published atomically.
#[test]
fn publish_batch_reports_published_files_on_midway_failure() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let first = base.path().join("first.json");
    let blocked = base.path().join("blocked");
    std::fs::create_dir(&blocked).expect("create the blocking directory");

    let options = PublishOptions { overwrite: OverwritePolicy::Replace, fsync_file: false, fsync_parent: false };
    let failure =
        publish_batch(&[PublishEntry { target: first.clone(), bytes: b"a" }, PublishEntry { target: blocked.clone(), bytes: b"b" }], options)
            .expect_err("renaming onto a directory fails");
    assert_eq!(failure.target, blocked);
    assert_eq!(failure.published, vec![first.clone()]);
    assert_eq!(read_to_string(&first).expect("read"), "a");
    assert!(blocked.is_dir());

    let leftovers: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("first.json"), std::ffi::OsString::from("blocked")]);
}

/// A clean batch publishes every destination in order and leaves only those files behind.
#[test]
fn publish_batch_writes_all_entries() {
    let base = boost_fs::temp::create_folder().expect("temp folder");
    let first = base.path().join("first.json");
    let second = base.path().join("second.json");

    let published = publish_batch(
        &[PublishEntry { target: first.clone(), bytes: b"a" }, PublishEntry { target: second.clone(), bytes: b"b" }],
        PublishOptions::default(),
    )
    .expect("publish batch");
    assert_eq!(published, vec![first.clone(), second.clone()]);
    assert_eq!(read_to_string(&first).expect("read first"), "a");
    assert_eq!(read_to_string(&second).expect("read second"), "b");

    let entries: Vec<_> = base.path().read_dir().expect("read dir").map(|entry| entry.expect("dir entry").file_name()).collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("first.json"), std::ffi::OsString::from("second.json")]);
}
