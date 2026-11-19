//! 宿主卷发布面：主文件、旁车与 CAS blob 的字节存储之上的提交语义。
//!
//! 宿主（文件系统、OPFS、内存映射或任何字节存储）只通过 [`HostVolumeStore`]
//! 提供寻址读写与持久化屏障，不实现提交顺序、配额判断或 revision 推进。
//! [`VolumePublisher`] 拥有发布语义：先校验镜像，再一次性写入并越过单一
//! 持久化屏障，最后才承认新的 generation。宿主无法提供某种原语时，实现方
//! 必须返回 [`database_types::Error::NativeCapabilityMissing`]，不得用内存状态
//! 冒充成功的发布。

use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use database_formats::yy::yydb::{
    PAGE_MAGIC, PAGE_SIZE,
    header::{DatabaseHeader, parse_page0_checked},
};
use database_types::{Error, Result};

use crate::local::journal::{shm_path, wal_path};

/// 单个持久卷内的逻辑文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeFile {
    /// `YDPG` 主文件。
    Main,
    /// `YYWL` WAL 旁车。
    Wal,
    /// `YYSH` SHM 旁车。
    Shm,
    /// `.yydx` CAS blob 段，按内容哈希寻址。
    Blob(String),
}

impl VolumeFile {
    /// `.yydx` blob 段角色。
    pub fn blob(hash: impl Into<String>) -> Self {
        Self::Blob(hash.into())
    }

    /// 诊断用角色标签。
    pub fn label(&self) -> &str {
        match self {
            Self::Main => "main",
            Self::Wal => "wal",
            Self::Shm => "shm",
            Self::Blob(_) => "blob",
        }
    }
}

/// 宿主提供的字节存储。
///
/// 只回答「读到什么」与「写到哪」，不做提交合并、同步顺序或配额策略。
/// 长度为 `0` 与不存在等价：宿主删除文件后可用截断到零表示。
pub trait HostVolumeStore {
    /// 文件当前长度；不存在返回 `None`。
    fn len(&mut self, file: &VolumeFile) -> Result<Option<u64>>;

    /// 从 `offset` 起读取，返回实际读取字节数。
    fn read_at(&mut self, file: &VolumeFile, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// 从 `offset` 起写入 `body` 的全部字节。
    fn write_at(&mut self, file: &VolumeFile, offset: u64, body: &[u8]) -> Result<()>;

    /// 截断或扩展文件到 `len` 字节。
    fn truncate(&mut self, file: &VolumeFile, len: u64) -> Result<()>;

    /// 对 `file` 已经写入的字节执行持久化屏障。
    fn sync(&mut self, file: &VolumeFile) -> Result<()>;

    /// 删除文件；宿主无法删除时必须返回显式错误。
    fn remove(&mut self, file: &VolumeFile) -> Result<()>;

    /// `(已用字节, 上限字节)`；宿主不上报配额时返回 `None`。
    fn quota(&mut self) -> Result<Option<(u64, u64)>> {
        Ok(None)
    }
}

/// 文件系统参考实现：`{path}`、`{path}-wal`、`{path}-shm` 与 `<stem>-objects/`。
#[derive(Debug, Clone)]
pub struct FsVolumeStore {
    db: PathBuf,
}

impl FsVolumeStore {
    /// 绑定一个数据库路径；仅解析名字，不触碰文件系统。
    pub fn open(db: impl Into<PathBuf>) -> Self {
        Self { db: db.into() }
    }

    /// 绑定到已存在的词法路径。
    pub fn path(&self) -> &Path {
        self.db.as_path()
    }

    /// `.yydx` 对象目录根：`<stem>-objects`（与主文件同级）。
    pub fn blob_root(&self) -> PathBuf {
        let parent = self.db.parent().unwrap_or_else(|| Path::new("."));
        let stem = self.db.file_stem().and_then(|name| name.to_str()).unwrap_or("app");
        parent.join(format!("{stem}-objects"))
    }

    fn file_path(&self, file: &VolumeFile) -> PathBuf {
        match file {
            VolumeFile::Main => self.db.clone(),
            VolumeFile::Wal => wal_path(&self.db),
            VolumeFile::Shm => shm_path(&self.db),
            VolumeFile::Blob(hash) => {
                let prefix: String = hash.chars().take(2).collect();
                self.blob_root().join(prefix).join(format!("{hash}.blob"))
            }
        }
    }
}

impl HostVolumeStore for FsVolumeStore {
    fn len(&mut self, file: &VolumeFile) -> Result<Option<u64>> {
        match std::fs::metadata(self.file_path(file)) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::Io(error)),
        }
    }

    fn read_at(&mut self, file: &VolumeFile, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let path = self.file_path(file);
        if !path.exists() {
            return Ok(0);
        }
        let mut handle = File::open(&path)?;
        handle.seek(SeekFrom::Start(offset))?;
        let mut read = 0_usize;
        while read < buf.len() {
            match handle.read(&mut buf[read..])? {
                0 => break,
                filled => read += filled,
            }
        }
        Ok(read)
    }

    fn write_at(&mut self, file: &VolumeFile, offset: u64, body: &[u8]) -> Result<()> {
        let path = self.file_path(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut handle = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path)?;
        handle.seek(SeekFrom::Start(offset))?;
        handle.write_all(body)?;
        Ok(())
    }

    fn truncate(&mut self, file: &VolumeFile, len: u64) -> Result<()> {
        let path = self.file_path(file);
        if !path.exists() {
            if len == 0 {
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            File::create(&path)?;
        }
        OpenOptions::new().write(true).open(&path)?.set_len(len)?;
        Ok(())
    }

    fn sync(&mut self, file: &VolumeFile) -> Result<()> {
        let path = self.file_path(file);
        if !path.exists() {
            return Ok(());
        }
        OpenOptions::new().write(true).open(&path)?.sync_all()?;
        Ok(())
    }

    fn remove(&mut self, file: &VolumeFile) -> Result<()> {
        match std::fs::remove_file(self.file_path(file)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }
}

/// Boxed stores forward every call, so hosts can erase the concrete store type.
impl<T: HostVolumeStore + ?Sized> HostVolumeStore for Box<T> {
    fn len(&mut self, file: &VolumeFile) -> Result<Option<u64>> {
        (**self).len(file)
    }

    fn read_at(&mut self, file: &VolumeFile, offset: u64, buf: &mut [u8]) -> Result<usize> {
        (**self).read_at(file, offset, buf)
    }

    fn write_at(&mut self, file: &VolumeFile, offset: u64, body: &[u8]) -> Result<()> {
        (**self).write_at(file, offset, body)
    }

    fn truncate(&mut self, file: &VolumeFile, len: u64) -> Result<()> {
        (**self).truncate(file, len)
    }

    fn sync(&mut self, file: &VolumeFile) -> Result<()> {
        (**self).sync(file)
    }

    fn remove(&mut self, file: &VolumeFile) -> Result<()> {
        (**self).remove(file)
    }

    fn quota(&mut self) -> Result<Option<(u64, u64)>> {
        (**self).quota()
    }
}

/// 宿主 blob 目录端口：段名枚举与按名删除无法用单文件句柄表达。
///
/// 主文件与旁车可以只靠 [`HostVolumeStore`] 的寻址读写完成，但 `.yydx` 的
/// 不可变段还需要「目录里现在有什么」和「删掉这一段」。浏览器 OPFS 只能以
/// 异步目录 API（`values()` / `removeEntry()`）提供这两件事，宿主在进入同步
/// 调用前完成解析，本端口只承载解析结果与删除命令，不做提交顺序判断。
///
/// 段名就是 [`VolumeFile::Blob`] 的哈希，目录内部如何分片（前缀目录、平铺）
/// 由宿主自己决定。
pub trait HostBlobDirectory {
    /// 目录内已存在的 blob 哈希，按字典序。
    fn list(&mut self) -> Result<Vec<String>>;

    /// 删除一个 blob 段；不存在视为成功。
    fn remove(&mut self, hash: &str) -> Result<()>;
}

/// 文件系统参考实现：枚举 `<stem>-objects/` 下的 `<prefix>/<hash>.blob`。
#[derive(Debug, Clone)]
pub struct FsBlobDirectory {
    blobs: FsVolumeStore,
    root: PathBuf,
}

impl FsBlobDirectory {
    /// 绑定 `<db>` 对应的 sibling 对象目录；仅解析名字，不触碰文件系统。
    pub fn open(db: impl Into<PathBuf>) -> Self {
        let blobs = FsVolumeStore::open(db);
        let root = blobs.blob_root();
        Self { blobs, root }
    }

    /// 对象目录根路径。
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }
}

impl HostBlobDirectory for FsBlobDirectory {
    fn list(&mut self) -> Result<Vec<String>> {
        let mut found = BTreeSet::new();
        collect_blob_hashes(&self.root, &mut found)?;
        Ok(found.into_iter().collect())
    }

    fn remove(&mut self, hash: &str) -> Result<()> {
        self.blobs.remove(&VolumeFile::blob(hash))
    }
}

impl<T: HostBlobDirectory + ?Sized> HostBlobDirectory for Box<T> {
    fn list(&mut self) -> Result<Vec<String>> {
        (**self).list()
    }

    fn remove(&mut self, hash: &str) -> Result<()> {
        (**self).remove(hash)
    }
}

fn collect_blob_hashes(dir: &Path, found: &mut BTreeSet<String>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_blob_hashes(&path, found)?;
        }
        else if path.extension().and_then(|ext| ext.to_str()) == Some("blob") {
            if let Some(hash) = path.file_stem().and_then(|stem| stem.to_str()) {
                found.insert(hash.to_owned());
            }
        }
    }
    Ok(())
}

/// 单个写入调用交给宿主的字节上限。
const WRITE_CHUNK_BYTES: usize = 1 << 20;

/// 一个持久卷的提交面：校验镜像、写入、越过持久化屏障、推进 generation。
pub struct VolumePublisher<S: HostVolumeStore> {
    path: String,
    store: S,
    blobs: Option<Box<dyn HostBlobDirectory>>,
    generation: u64,
}

impl<S: HostVolumeStore> VolumePublisher<S> {
    /// 绑定逻辑路径与宿主存储，并从已存在的主文件读回当前 generation。
    pub fn open(path: impl Into<String>, mut store: S) -> Result<Self> {
        let path = path.into();
        let generation = match read_whole(&mut store, &VolumeFile::Main)? {
            Some(bytes) => main_header(&bytes)?.slot.generation,
            None => 0,
        };
        Ok(Self { path, store, blobs: None, generation })
    }

    /// 绑定宿主 blob 目录；`.yydx` 的枚举与删除改由该端口承担。
    pub fn with_blob_directory(mut self, blobs: Box<dyn HostBlobDirectory>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// 是否绑定了宿主 blob 目录。
    pub fn has_blob_directory(&self) -> bool {
        self.blobs.is_some()
    }

    /// 逻辑数据库路径（例如 `app.yydb`）。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 最近一次成功发布的主文件 generation（无主文件时为 `0`）。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 宿主存储的只读引用。
    pub fn store(&self) -> &S {
        &self.store
    }

    /// 读回持久主文件镜像。
    pub fn read_main(&mut self) -> Result<Option<Vec<u8>>> {
        read_whole(&mut self.store, &VolumeFile::Main)
    }

    /// 发布一版主文件镜像，返回该镜像的 generation。
    ///
    /// 顺序固定：校验镜像 → 配额判断 → 写入 → 单一持久化屏障。任一环节失败
    /// 都不会推进 [`Self::generation`]，调用方据此判断发布是否已被承认。
    pub fn publish_main(&mut self, body: &[u8]) -> Result<u64> {
        let header = main_header(body)?;
        let previous = self.store.len(&VolumeFile::Main)?.unwrap_or(0);
        enforce_quota(&mut self.store, &self.path, previous, body.len() as u64)?;
        write_whole(&mut self.store, &VolumeFile::Main, body)?;
        self.store.sync(&VolumeFile::Main)?;
        self.generation = header.slot.generation;
        Ok(self.generation)
    }

    /// 读回 `YYWL` 旁车。
    pub fn read_wal(&mut self) -> Result<Option<Vec<u8>>> {
        read_whole(&mut self.store, &VolumeFile::Wal)
    }

    /// 发布 `YYWL` 旁车并越过持久化屏障。
    pub fn publish_wal(&mut self, body: &[u8]) -> Result<()> {
        write_whole(&mut self.store, &VolumeFile::Wal, body)?;
        self.store.sync(&VolumeFile::Wal)
    }

    /// 删除 `YYWL` 旁车。
    pub fn remove_wal(&mut self) -> Result<()> {
        self.store.remove(&VolumeFile::Wal)
    }

    /// 读回 `YYSH` 旁车。
    pub fn read_shm(&mut self) -> Result<Option<Vec<u8>>> {
        read_whole(&mut self.store, &VolumeFile::Shm)
    }

    /// 发布 `YYSH` 旁车并越过持久化屏障。
    pub fn publish_shm(&mut self, body: &[u8]) -> Result<()> {
        write_whole(&mut self.store, &VolumeFile::Shm, body)?;
        self.store.sync(&VolumeFile::Shm)
    }

    /// 删除 `YYSH` 旁车。
    pub fn remove_shm(&mut self) -> Result<()> {
        self.store.remove(&VolumeFile::Shm)
    }

    /// 读取一个已发布的 blob 段。
    pub fn read_blob(&mut self, hash: &str) -> Result<Option<Vec<u8>>> {
        read_whole(&mut self.store, &VolumeFile::blob(hash))
    }

    /// 发布一个不可变 blob 段；同哈希不同内容返回 [`Error::CasConflict`]。
    pub fn publish_blob(&mut self, hash: &str, body: &[u8]) -> Result<()> {
        let file = VolumeFile::blob(hash);
        if let Some(existing) = read_whole(&mut self.store, &file)? {
            if existing == body {
                return Ok(());
            }
            return Err(Error::CasConflict { key: format!("blob:{hash}") });
        }
        write_whole(&mut self.store, &file, body)?;
        self.store.sync(&file)
    }

    /// 删除一个 blob 段。
    ///
    /// 绑定了宿主目录时由目录端口负责（OPFS 只有异步 `removeEntry()` 能删），
    /// 否则退回 [`HostVolumeStore::remove`] 的单文件语义。
    pub fn remove_blob(&mut self, hash: &str) -> Result<()> {
        match self.blobs.as_deref_mut() {
            Some(directory) => directory.remove(hash),
            None => self.store.remove(&VolumeFile::blob(hash)),
        }
    }

    /// 枚举已发布的 blob 段。
    ///
    /// 只有宿主目录端口能回答「目录里现在有什么」；没有目录时显式拒绝，
    /// 不把未知当成空目录。
    pub fn list_blobs(&mut self) -> Result<Vec<String>> {
        match self.blobs.as_deref_mut() {
            Some(directory) => directory.list(),
            None => Err(Error::NativeCapabilityMissing { capability: "volume.blob_directory" }),
        }
    }
}

fn main_header(body: &[u8]) -> Result<DatabaseHeader> {
    if body.len() < PAGE_SIZE {
        return Err(Error::Corrupt("main image is shorter than one page"));
    }
    if body.len() % PAGE_SIZE != 0 {
        return Err(Error::Corrupt("main image length is not page aligned"));
    }
    if body.get(0..PAGE_MAGIC.len()) != Some(PAGE_MAGIC.as_slice()) {
        return Err(Error::Unsupported("main image is not YDPG format v0"));
    }
    parse_page0_checked(&body[..PAGE_SIZE])
}

fn enforce_quota<S: HostVolumeStore>(store: &mut S, path: &str, previous: u64, incoming: u64) -> Result<()> {
    let Some((used, limit)) = store.quota()?
    else {
        return Ok(());
    };
    let projected = used.saturating_sub(previous).saturating_add(incoming);
    if projected > limit {
        return Err(Error::QuotaExceeded { namespace: format!("volume:{path}") });
    }
    Ok(())
}

fn read_whole<S: HostVolumeStore>(store: &mut S, file: &VolumeFile) -> Result<Option<Vec<u8>>> {
    let Some(len) = store.len(file)?
    else {
        return Ok(None);
    };
    if len == 0 {
        return Ok(None);
    }
    let len = usize::try_from(len).map_err(|_| Error::Corrupt("host file is larger than addressable memory"))?;
    let mut body = vec![0_u8; len];
    let mut filled = 0_usize;
    while filled < len {
        match store.read_at(file, filled as u64, &mut body[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    body.truncate(filled);
    Ok(Some(body))
}

fn write_whole<S: HostVolumeStore>(store: &mut S, file: &VolumeFile, body: &[u8]) -> Result<()> {
    for (index, chunk) in body.chunks(WRITE_CHUNK_BYTES).enumerate() {
        store.write_at(file, (index * WRITE_CHUNK_BYTES) as u64, chunk)?;
    }
    store.truncate(file, body.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("local-volume-dir-{name}-{}.yydx", std::process::id()))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(wal_path(path));
        let _ = std::fs::remove_file(shm_path(path));
        if let Some(parent) = path.parent() {
            let stem = path.file_stem().and_then(|name| name.to_str()).unwrap_or("app");
            let _ = std::fs::remove_dir_all(parent.join(format!("{stem}-objects")));
        }
    }

    #[test]
    fn blob_directory_lists_and_removes_published_segments() {
        let path = temp_db("roundtrip");
        cleanup(&path);
        let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path))
            .unwrap()
            .with_blob_directory(Box::new(FsBlobDirectory::open(&path)));
        assert!(publisher.has_blob_directory());

        publisher.publish_blob("abc1", b"first").unwrap();
        publisher.publish_blob("abc2", b"second").unwrap();
        assert_eq!(publisher.list_blobs().unwrap(), vec!["abc1".to_owned(), "abc2".to_owned()]);

        publisher.remove_blob("abc1").unwrap();
        assert_eq!(publisher.list_blobs().unwrap(), vec!["abc2".to_owned()]);
        assert_eq!(publisher.read_blob("abc1").unwrap(), None, "a removed segment must read as absent");
        assert_eq!(publisher.read_blob("abc2").unwrap(), Some(b"second".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn listing_blobs_without_a_directory_is_a_capability_gap() {
        let path = temp_db("no-directory");
        cleanup(&path);
        let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
        assert!(!publisher.has_blob_directory());
        let error = publisher.list_blobs().expect_err("no directory cannot enumerate segments");
        assert!(
            matches!(error, Error::NativeCapabilityMissing { capability: "volume.blob_directory" }),
            "expected the blob directory capability gap, got {error}"
        );
        cleanup(&path);
    }

    #[test]
    fn removing_a_blob_without_a_directory_keeps_the_store_semantics() {
        let path = temp_db("store-remove");
        cleanup(&path);
        let mut publisher = VolumePublisher::open(path.to_string_lossy().to_string(), FsVolumeStore::open(&path)).unwrap();
        publisher.publish_blob("def1", b"body").unwrap();
        publisher.remove_blob("def1").unwrap();
        assert_eq!(publisher.read_blob("def1").unwrap(), None);
        cleanup(&path);
    }
}
