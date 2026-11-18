//! 宿主卷发布面：主文件、旁车与 CAS blob 的字节存储之上的提交语义。
//!
//! 宿主（文件系统、OPFS、内存映射或任何字节存储）只通过 [`HostVolumeStore`]
//! 提供寻址读写与持久化屏障，不实现提交顺序、配额判断或 revision 推进。
//! [`VolumePublisher`] 拥有发布语义：先校验镜像，再一次性写入并越过单一
//! 持久化屏障，最后才承认新的 generation。宿主无法提供某种原语时，实现方
//! 必须返回 [`database_types::Error::NativeCapabilityMissing`]，不得用内存状态
//! 冒充成功的发布。

use std::{
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

    fn file_path(&self, file: &VolumeFile) -> PathBuf {
        match file {
            VolumeFile::Main => self.db.clone(),
            VolumeFile::Wal => wal_path(&self.db),
            VolumeFile::Shm => shm_path(&self.db),
            VolumeFile::Blob(hash) => {
                let prefix: String = hash.chars().take(2).collect();
                let parent = self.db.parent().unwrap_or_else(|| Path::new("."));
                let stem = self.db.file_stem().and_then(|name| name.to_str()).unwrap_or("app");
                parent.join(format!("{stem}-objects")).join(prefix).join(format!("{hash}.blob"))
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

/// 单个写入调用交给宿主的字节上限。
const WRITE_CHUNK_BYTES: usize = 1 << 20;

/// 一个持久卷的提交面：校验镜像、写入、越过持久化屏障、推进 generation。
pub struct VolumePublisher<S: HostVolumeStore> {
    path: String,
    store: S,
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
        Ok(Self { path, store, generation })
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
    pub fn remove_blob(&mut self, hash: &str) -> Result<()> {
        self.store.remove(&VolumeFile::blob(hash))
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
