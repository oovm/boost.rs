//! 宿主卷发布语义：真实字节存储上的提交、屏障、配额与 CAS。

use std::path::{Path, PathBuf};

use database_storage::local_io::{
    FsVolumeStore, HostVolumeStore, MemoryPager, VolumeFile, VolumePublisher, main_bytes_from_memory_pager, shm_sidecar_path, wal_sidecar_path,
};
use database_types::Error;

fn temp_db(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("local-volume-{name}-{}.yydb", std::process::id()))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_sidecar_path(path));
    let _ = std::fs::remove_file(shm_sidecar_path(path));
    if let Some(parent) = path.parent() {
        let stem = path.file_stem().and_then(|name| name.to_str()).unwrap_or("app");
        let _ = std::fs::remove_dir_all(parent.join(format!("{stem}-objects")));
    }
}

fn main_image(tag: u8) -> Vec<u8> {
    let mut id = [0xAA_u8; 16];
    id[0] = tag;
    let pager = MemoryPager::new_empty(id, 0x01);
    main_bytes_from_memory_pager(&pager).unwrap()
}

#[test]
fn publish_main_roundtrips_through_host_bytes() {
    let path = temp_db("publish");
    cleanup(&path);

    let body = main_image(1);
    {
        let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
        assert_eq!(publisher.read_main().unwrap(), None);
        publisher.publish_main(&body).unwrap();
        assert_eq!(publisher.read_main().unwrap(), Some(body.clone()));
    }

    let mut reopened = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
    assert_eq!(reopened.read_main().unwrap(), Some(body));
    assert_eq!(&std::fs::read(&path).unwrap(), &std::fs::read(&path).unwrap());
    cleanup(&path);
}

#[test]
fn publish_main_rejects_a_non_ydpg_image_without_touching_the_volume() {
    let path = temp_db("reject");
    cleanup(&path);
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
    publisher.publish_main(&main_image(2)).unwrap();

    let error = publisher.publish_main(b"not-a-ydpg-image").unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "expected Corrupt, got {error}");
    assert_eq!(&publisher.read_main().unwrap().unwrap()[0..5], b"YDPG\0");
    cleanup(&path);
}

#[test]
fn sidecars_publish_read_and_remove() {
    let path = temp_db("sidecars");
    cleanup(&path);
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();

    publisher.publish_wal(b"YYWL-sidecar").unwrap();
    publisher.publish_shm(b"YYSH-sidecar").unwrap();
    assert_eq!(publisher.read_wal().unwrap(), Some(b"YYWL-sidecar".to_vec()));
    assert_eq!(publisher.read_shm().unwrap(), Some(b"YYSH-sidecar".to_vec()));

    publisher.remove_wal().unwrap();
    publisher.remove_shm().unwrap();
    assert_eq!(publisher.read_wal().unwrap(), None);
    assert_eq!(publisher.read_shm().unwrap(), None);
    assert!(!wal_sidecar_path(&path).exists());
    assert!(!shm_sidecar_path(&path).exists());
    cleanup(&path);
}

#[test]
fn blob_publish_is_content_addressed() {
    let path = temp_db("blob");
    cleanup(&path);
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();

    publisher.publish_blob("chunk-a", b"payload").unwrap();
    assert_eq!(publisher.read_blob("chunk-a").unwrap(), Some(b"payload".to_vec()));
    publisher.publish_blob("chunk-a", b"payload").unwrap();

    let error = publisher.publish_blob("chunk-a", b"other").unwrap_err();
    assert!(matches!(error, Error::CasConflict { .. }), "expected CasConflict, got {error}");
    cleanup(&path);
}

/// 宿主上报配额时，超额发布必须被拒绝且不改变已提交内容。
struct QuotaStore {
    inner: FsVolumeStore,
    limit: u64,
}

impl HostVolumeStore for QuotaStore {
    fn len(&mut self, file: &VolumeFile) -> database_types::Result<Option<u64>> {
        self.inner.len(file)
    }

    fn read_at(&mut self, file: &VolumeFile, offset: u64, buf: &mut [u8]) -> database_types::Result<usize> {
        self.inner.read_at(file, offset, buf)
    }

    fn write_at(&mut self, file: &VolumeFile, offset: u64, body: &[u8]) -> database_types::Result<()> {
        self.inner.write_at(file, offset, body)
    }

    fn truncate(&mut self, file: &VolumeFile, len: u64) -> database_types::Result<()> {
        self.inner.truncate(file, len)
    }

    fn sync(&mut self, file: &VolumeFile) -> database_types::Result<()> {
        self.inner.sync(file)
    }

    fn remove(&mut self, file: &VolumeFile) -> database_types::Result<()> {
        self.inner.remove(file)
    }

    fn quota(&mut self) -> database_types::Result<Option<(u64, u64)>> {
        Ok(Some((self.inner.len(&VolumeFile::Main)?.unwrap_or(0), self.limit)))
    }
}

#[test]
fn quota_exhaustion_is_reported_without_publishing() {
    let path = temp_db("quota");
    cleanup(&path);
    let first = main_image(3);
    let store = QuotaStore { inner: FsVolumeStore::open(&path), limit: first.len() as u64 };
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), store).unwrap();
    publisher.publish_main(&first).unwrap();

    let mut oversized = main_image(4);
    oversized.extend_from_slice(&vec![0_u8; 4096]);
    let error = publisher.publish_main(&oversized).unwrap_err();
    assert!(matches!(error, Error::QuotaExceeded { .. }), "expected QuotaExceeded, got {error}");
    assert_eq!(publisher.read_main().unwrap(), Some(first));
    cleanup(&path);
}

/// 宿主无法删除文件时必须报出缺失能力，不得静默成功。
struct NoRemoveStore {
    inner: FsVolumeStore,
}

impl HostVolumeStore for NoRemoveStore {
    fn len(&mut self, file: &VolumeFile) -> database_types::Result<Option<u64>> {
        self.inner.len(file)
    }

    fn read_at(&mut self, file: &VolumeFile, offset: u64, buf: &mut [u8]) -> database_types::Result<usize> {
        self.inner.read_at(file, offset, buf)
    }

    fn write_at(&mut self, file: &VolumeFile, offset: u64, body: &[u8]) -> database_types::Result<()> {
        self.inner.write_at(file, offset, body)
    }

    fn truncate(&mut self, file: &VolumeFile, len: u64) -> database_types::Result<()> {
        self.inner.truncate(file, len)
    }

    fn sync(&mut self, file: &VolumeFile) -> database_types::Result<()> {
        self.inner.sync(file)
    }

    fn remove(&mut self, _file: &VolumeFile) -> database_types::Result<()> {
        Err(Error::NativeCapabilityMissing { capability: "volume.unlink" })
    }
}

#[test]
fn missing_host_capability_surfaces_as_a_structured_error() {
    let path = temp_db("capability");
    cleanup(&path);
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), NoRemoveStore { inner: FsVolumeStore::open(&path) }).unwrap();

    let error = publisher.remove_wal().unwrap_err();
    match error {
        Error::NativeCapabilityMissing { capability } => assert_eq!(capability, "volume.unlink"),
        other => panic!("expected NativeCapabilityMissing, got {other}"),
    }
    cleanup(&path);
}

#[test]
fn empty_volume_reads_as_absent() {
    let path = temp_db("empty");
    cleanup(&path);
    let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
    assert_eq!(publisher.read_main().unwrap(), None);
    assert_eq!(publisher.generation(), 0);
    cleanup(&path);
}
