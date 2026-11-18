//! `.yykv` 分片所有权：一个分片文件按逻辑数据库划分命名空间，承载 Redis 键空间。
//!
//! 键空间、值种类、revision、过期截止时间、`WATCH` 版本与持久化边界都归这里；
//! 集合命令语义归 `database.execution`，绑定层只做参数与结果映射。

pub mod format;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs, io,
    path::{Path, PathBuf},
};

use format::{Key, Record, Records};

pub use format::{
    CatalogError, ShardFormatError, ShardResult, decode_catalog_shard_id, decode_shard, encode_catalog, encode_shard, record_bytes,
};

use super::store::now_unix_ms;

/// 服务暴露的逻辑数据库个数，与 Redis 默认值一致。
pub const DATABASES: u8 = 16;
/// 数据目录内分片快照的文件名。
pub const SHARD_FILE: &str = "local-0.yykv";
/// 数据目录内 catalog 的文件名。
pub const CATALOG_FILE: &str = "local-0.yyds";
/// 携带逻辑数据库序号的命名空间前缀。
pub const NAMESPACE_PREFIX: &str = "redis:db";
/// 一个逻辑键的值种类，按探测顺序排列。
///
/// 字符串种类沿用数据库命名空间，使既有数据无需迁移，其余种类各自追加后缀。
/// 六个命名空间互不重叠，因此同一个逻辑键在任一时刻只能被读成一种值种类。
pub const KINDS: [&str; 6] = ["", "hash", "list", "set", "zset", "stream"];

/// 校验值种类名是否属于分片承认的六种。
pub fn check_kind(kind: &str) -> Result<(), String> {
    KINDS.contains(&kind).then_some(()).ok_or_else(|| format!("unknown value kind `{kind}`"))
}

/// 一次清扫因截止时间已到而删除的一条逻辑键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredKey {
    /// 键所在的数据库序号。
    pub db: u8,
    /// 不透明键字节。
    pub key: Vec<u8>,
}

/// 一次 `RENAME` 类移动的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// 源键存在，移动已生效。
    Renamed,
    /// 源键不存在。
    Missing,
    /// `RENAMENX` 发现目标已被占用。
    DestinationPresent,
}

/// 递增失败的类型化原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrementError {
    /// 现值不是规范十进制整数。
    NotAnInteger,
    /// 增减结果溢出 `i64`。
    Overflow,
}

impl std::fmt::Display for IncrementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotAnInteger => "value is not an integer or out of range",
            Self::Overflow => "increment or decrement would overflow",
        })
    }
}

impl std::error::Error for IncrementError {}

/// 磁盘上的一个 `.yyds` catalog 加一个 `.yykv` 分片。
///
/// `.yykv` 快照对整个 body 取一次校验和，并在记录前写入记录数，因此重写任意一条记录
/// 都要重新序列化整个分片。提交因此原地修改记录映射再序列化同一映射，而不是重建第二份。
/// 计数更新先累积在内存，在 [`RedisShard::flush`]、任何其他改动或节点析构时才落到文件；
/// 持久化边界见 [`RedisShard::flush`]。
///
/// [`RedisShard::begin_batch`] 与 [`RedisShard::end_batch`] 包住共享一次 socket 读取的
/// 一串命令：批内每次改动立即落到内存，分片在最外层批次关闭时只编码一次。
/// [`RedisShard::end_batch_deferred`] 同样关闭批次但不写盘，让带 flush 定时器的宿主把
/// 顺序命令流留在快照路径之外；`sync_always` 下它仍会写穿。计数更新在两种情况下都留在
/// 缓冲区，直到一次已提交改动、一次显式 [`RedisShard::flush`] 或析构，因此顺序 `INCR`
/// 流不会逐条命令重写快照。读取总是看到最新状态，批内顺序不改变可观测结果。
#[derive(Debug)]
pub struct RedisShard {
    data_dir: PathBuf,
    shard_path: PathBuf,
    records: Records,
    next_revision: u64,
    pending_increments: bool,
    /// 批次内发生过尚未落盘的改动。
    dirty: bool,
    /// 已打开批次的嵌套深度；为零时每次改动都写穿。
    batch_depth: u32,
    /// 每次快照写入之后是否跟随 `fsync`。
    ///
    /// `false` 即 `everysec` 策略：快照每次写入都到达文件系统，周期性的
    /// [`RedisShard::sync`] 才把它刷到稳定存储，因此断电最多丢掉上次 sync 之后的写入。
    /// 进程崩溃不会丢数据，因为文件已持有页缓存中的数据。
    sync_always: bool,
    /// 上次快照写入未同步。
    unsynced: bool,
    /// 被 `WATCH` 钉住的键及其改动版本和引用计数。
    ///
    /// 只跟踪被观察的键，映射规模由活跃 watch 决定。任何写入被观察逻辑键的提交都会
    /// 递增它的版本，包括逐字节相同的重写——这正是 Redis 视为「被修改」的信号。
    watched: HashMap<(String, Vec<u8>), (u64, u32)>,
}

impl RedisShard {
    /// 打开存储目录；catalog 或空分片缺失时按需创建。
    ///
    /// 既有 catalog 在任何读写之前被完整校验：magic、格式版本、保留段、分片数、
    /// 分片 id 形状、精确长度，以及它登记的分片是否存在。每个失败都在消息里点名具体
    /// 字段。catalog 缺失时保留旧行为：从推导路径读取或创建分片，再围绕它创建 catalog。
    ///
    /// 分片先写、catalog 后写，因为 catalog 是对分片的登记。两次 rename 之间崩溃只会
    /// 留下一个下次打开时围绕既有分片重建 catalog 的目录，而不是一条没有数据的登记。
    pub fn open(data_dir: impl AsRef<Path>) -> io::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;
        let catalog_path = data_dir.join(CATALOG_FILE);
        let registered = read_catalog(&catalog_path)?;
        let shard_id = match &registered {
            Some(id) => id.clone(),
            None => "local-0".to_owned(),
        };
        let shard_path = data_dir.join(format!("{shard_id}.yykv"));
        let (records, next_revision) = if shard_path.exists() {
            let bytes = fs::read(&shard_path)?;
            decode_shard(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
        }
        else if registered.is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("catalog.shard-file: `{shard_id}.yykv` is missing")));
        }
        else {
            let records = Records::new();
            write_durable(&shard_path, &encode(&records, 1)?, true)?;
            (records, 1)
        };
        if registered.is_none() {
            write_durable(&catalog_path, &encode_catalog("local-0"), true)?;
        }
        Ok(Self {
            data_dir,
            shard_path,
            records,
            next_revision: next_revision.max(1),
            pending_increments: false,
            dirty: false,
            batch_depth: 0,
            sync_always: false,
            unsynced: false,
            watched: HashMap::new(),
        })
    }

    /// 保存 catalog 与分片的目录。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 承载数据的 `.yykv` 路径。
    pub fn shard_path(&self) -> &Path {
        &self.shard_path
    }

    /// 数据库 `db` 中某一种值种类的 `key` 的活值。
    pub fn get_kind(&self, db: u8, kind: &str, key: &[u8]) -> Option<&Record> {
        let now = now_unix_ms();
        self.records.get(&kind_key(db, kind, key)).filter(|record| !record.is_expired_at(now))
    }

    /// 某一种值种类的剩余截止时间（毫秒），没有截止时间时为 `None`。
    pub fn deadline_kind(&self, db: u8, kind: &str, key: &[u8]) -> Option<i64> {
        self.get_kind(db, kind, key).and_then(|record| record.expires_at)
    }

    /// 键的某一种值种类是否存在且活着。
    pub fn contains_kind(&self, db: u8, kind: &str, key: &[u8]) -> bool {
        self.get_kind(db, kind, key).is_some()
    }

    /// 插入或替换键的某一种值种类，可带墙钟截止时间。
    pub fn set_kind(&mut self, db: u8, kind: &str, key: &[u8], value: &[u8], expires_at: Option<i64>) -> io::Result<()> {
        self.commit(vec![Change::Put { key: kind_key(db, kind, key), value: value.to_vec(), expires_at }])
    }

    /// 插入或替换某一种值种类，并返回分配给该记录的 revision。
    pub fn set_kind_revision(&mut self, db: u8, kind: &str, key: &[u8], value: &[u8], expires_at: Option<i64>) -> io::Result<u64> {
        let revision = self.next_revision;
        self.set_kind(db, kind, key, value, expires_at)?;
        Ok(revision)
    }

    /// 删除键的某一种值种类，报告是否删掉了活记录。
    pub fn delete_kind(&mut self, db: u8, kind: &str, key: &[u8]) -> io::Result<bool> {
        let existed = self.contains_kind(db, kind, key);
        if existed {
            self.commit(vec![Change::Delete { key: kind_key(db, kind, key) }])?;
        }
        Ok(existed)
    }

    /// 设置或清除键的某一种活值种类的截止时间。
    pub fn set_deadline_kind(&mut self, db: u8, kind: &str, key: &[u8], expires_at: Option<i64>) -> io::Result<bool> {
        let Some(record) = self.get_kind(db, kind, key)
        else {
            return Ok(false);
        };
        let value = format::record_bytes(record).to_vec();
        self.commit(vec![Change::Put { key: kind_key(db, kind, key), value, expires_at }])?;
        Ok(true)
    }

    /// 数据库 `db` 中 `key` 的活字符串值。
    pub fn get(&self, db: u8, key: &[u8]) -> Option<&Record> {
        self.get_kind(db, "", key)
    }

    /// 字符串种类的剩余截止时间，没有截止时间时为 `None`。
    pub fn deadline(&self, db: u8, key: &[u8]) -> Option<i64> {
        self.deadline_kind(db, "", key)
    }

    /// 键的字符串种类是否存在且活着。
    pub fn contains(&self, db: u8, key: &[u8]) -> bool {
        self.contains_kind(db, "", key)
    }

    /// 插入或替换字符串种类，可带墙钟截止时间。
    pub fn set(&mut self, db: u8, key: &[u8], value: &[u8], expires_at: Option<i64>) -> io::Result<()> {
        self.set_kind(db, "", key, value, expires_at)
    }

    /// 删除字符串种类，报告是否删掉了活记录。
    pub fn delete(&mut self, db: u8, key: &[u8]) -> io::Result<bool> {
        self.delete_kind(db, "", key)
    }

    /// 设置或清除字符串种类的截止时间。
    pub fn set_deadline(&mut self, db: u8, key: &[u8], expires_at: Option<i64>) -> io::Result<bool> {
        self.set_deadline_kind(db, "", key, expires_at)
    }

    /// 当前持有 `key` 的值种类；所有种类命名空间都为空时为 `None`。
    ///
    /// 一个逻辑键在任一时刻只有一种表示活着，因此按 [`KINDS`] 探测顺序的第一个命中
    /// 就是完整答案。
    pub fn find_kind(&self, db: u8, key: &[u8]) -> Option<&'static str> {
        KINDS.into_iter().find(|kind| self.contains_kind(db, *kind, key))
    }

    /// 把 `source` 移到 `destination`，连同值种类与截止时间。
    ///
    /// `destination` 上已存在的其他种类先被删除，使目标只剩一种表示。`if_absent`
    /// 实现 `RENAMENX`。
    pub fn rename(&mut self, db: u8, source: &[u8], destination: &[u8], if_absent: bool) -> io::Result<RenameOutcome> {
        let Some(source_kind) = self.find_kind(db, source)
        else {
            return Ok(RenameOutcome::Missing);
        };
        if source == destination {
            return Ok(if if_absent { RenameOutcome::DestinationPresent } else { RenameOutcome::Renamed });
        }
        if if_absent && self.find_kind(db, destination).is_some() {
            return Ok(RenameOutcome::DestinationPresent);
        }
        let Some(record) = self.get_kind(db, source_kind, source)
        else {
            return Ok(RenameOutcome::Missing);
        };
        let value = format::record_bytes(record).to_vec();
        let expires_at = record.expires_at;
        let mut changes = KINDS.into_iter().map(|kind| Change::Delete { key: kind_key(db, kind, destination) }).collect::<Vec<_>>();
        changes.push(Change::Delete { key: kind_key(db, source_kind, source) });
        changes.push(Change::Put { key: kind_key(db, source_kind, destination), value, expires_at });
        self.commit(changes)?;
        Ok(RenameOutcome::Renamed)
    }

    /// 丢弃一个数据库里所有种类命名空间的活记录。
    pub fn flush_db(&mut self, db: u8) -> io::Result<()> {
        let base = namespace(db);
        let now = now_unix_ms();
        let mut changes = Vec::new();
        for (key, record) in &self.records {
            if belongs_to(&key.namespace.0, &base) && !record.is_expired_at(now) {
                changes.push(Change::Delete { key: key.clone() });
            }
        }
        if changes.is_empty() {
            return Ok(());
        }
        self.commit(changes)
    }

    /// 一个数据库的活逻辑键数量，每个键只计一次。
    pub fn len(&self, db: u8) -> usize {
        self.live_keys(db).len()
    }

    /// 数据库没有活键。
    pub fn is_empty(&self, db: u8) -> bool {
        self.len(db) == 0
    }

    /// 一个数据库的活逻辑键，按字节升序，支撑 `KEYS` 与 `SCAN`。
    ///
    /// 一个逻辑键只会活在五个种类命名空间之一，因此集合既去重又给出确定顺序。
    pub fn keys(&self, db: u8) -> Vec<Vec<u8>> {
        self.live_keys(db).into_iter().collect()
    }

    fn live_keys(&self, db: u8) -> BTreeSet<Vec<u8>> {
        let base = namespace(db);
        let now = now_unix_ms();
        self.records
            .iter()
            .filter(|(key, record)| belongs_to(&key.namespace.0, &base) && !record.is_expired_at(now))
            .map(|(key, _)| key.bytes.clone())
            .collect()
    }

    /// 用一次持久化提交插入或替换多个值，支撑批量装载。
    pub fn set_many(&mut self, db: u8, entries: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
        let base = namespace(db);
        let changes = entries
            .iter()
            .map(|(key, value)| Change::Put { key: Key::new(base.clone(), key.clone()), value: value.clone(), expires_at: None })
            .collect();
        self.commit(changes)
    }

    /// 在活值末尾追加 `suffix`，保留其截止时间。
    pub fn append(&mut self, db: u8, key: &[u8], suffix: &[u8]) -> io::Result<usize> {
        let mut value = self.get(db, key).map_or_else(Vec::new, |record| format::record_bytes(record).to_vec());
        value.extend_from_slice(suffix);
        let deadline = self.deadline(db, key);
        self.set(db, key, &value, deadline)?;
        Ok(value.len())
    }

    /// 移除所有在 `now_unix_ms` 已到截止时间的记录。
    ///
    /// 返回过期的不同逻辑键，按数据库与键字节排序。已过截止时间的记录对读取不可见但仍
    /// 占据分片，因此清扫删除的正是这些记录，而不是逐键探测键空间。删除是一次普通快照
    /// 提交，所以无事可删的一轮从不触碰文件。
    pub fn sweep_expired(&mut self, now_unix_ms: i64) -> io::Result<Vec<ExpiredKey>> {
        let mut expired = BTreeSet::new();
        let mut changes = Vec::new();
        for (key, record) in &self.records {
            if record.is_expired_at(now_unix_ms) {
                if let Some(db) = database_index(&key.namespace.0) {
                    expired.insert((db, key.bytes.clone()));
                }
                changes.push(Change::Delete { key: key.clone() });
            }
        }
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        self.commit(changes)?;
        Ok(expired.into_iter().map(|(db, key)| ExpiredKey { db, key }).collect())
    }

    /// 给整数值加上 `delta`，保留其截止时间。
    ///
    /// 结果对本节点上的每次读取立即可见。它在下一次 [`RedisShard::flush`]、任何其他改动
    /// 或节点析构时到达分片；在此之前只存在于内存。
    pub fn incr_by(&mut self, db: u8, key: &[u8], delta: i64) -> io::Result<std::result::Result<i64, IncrementError>> {
        let current = match self.get(db, key) {
            None => 0,
            Some(record) => match database_types::canonical::parse_i64(format::record_bytes(record)) {
                Some(value) => value,
                None => return Ok(Err(IncrementError::NotAnInteger)),
            },
        };
        let Some(total) = current.checked_add(delta)
        else {
            return Ok(Err(IncrementError::Overflow));
        };
        let expires_at = self.deadline(db, key);
        let revision = self.bump_revision();
        let record_key = Key::new(namespace(db), key.to_vec());
        let record = format::inline_record(record_key.clone(), total.to_string().into_bytes(), revision, expires_at);
        self.records.insert(record_key.clone(), record);
        self.touch_watched(&record_key);
        self.pending_increments = true;
        Ok(Ok(total))
    }

    /// 打开一个批次，让其中的改动共享一次持久化。
    ///
    /// 调用可嵌套。最外层批次内的每次改动立即落到内存，分片在最外层批次关闭时只编码一次。
    /// 读取从不等待文件，所以批内命令顺序保持不变。
    pub fn begin_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_add(1);
    }

    /// 关闭一个批次，并在最外层深度上写入已提交的改动。
    ///
    /// 计数更新留在缓冲区：它们在下一次已提交改动、[`RedisShard::flush`]（服务端的一秒
    /// 定时器与关停路径）或节点析构时到达分片。这样顺序 `INCR` 流不会逐条命令重写快照。
    pub fn end_batch(&mut self) -> io::Result<()> {
        if self.batch_depth > 0 {
            self.batch_depth -= 1;
        }
        if self.batch_depth == 0 && self.dirty {
            self.flush()?;
        }
        Ok(())
    }

    /// 关闭一个批次，把快照写入留给宿主的 flush 节奏。
    ///
    /// socket 服务端每轮只读一条命令，若每个批次都写盘就会逐条命令重写整份分片。写盘时机
    /// 改由宿主持有（[`RedisShard::flush`] 的持久化节奏、`SAVE` 与关停），与计数更新已经
    /// 使用的写后置边界一致。既改动又缓冲了计数的批次仍会写穿，因为已提交改动就是缓冲增量
    /// 到达分片的约定点。`sync_always` 下每个批次都写穿，因为该策略承诺每次提交都同步快照。
    pub fn end_batch_deferred(&mut self) -> io::Result<()> {
        if self.sync_always || (self.dirty && self.pending_increments) {
            return self.end_batch();
        }
        if self.batch_depth > 0 {
            self.batch_depth -= 1;
        }
        Ok(())
    }

    /// 登记一个逻辑键上的 watch，并返回它当前的版本。
    ///
    /// 同一键的重复 watch 通过引用计数共享一条记录，因此一个会话可以 watch 两次而不泄漏槽位。
    pub fn watch_key(&mut self, db: u8, key: &[u8]) -> u64 {
        let entry = (namespace(db), key.to_vec());
        let slot = self.watched.entry(entry).or_insert((0, 0));
        slot.1 = slot.1.saturating_add(1);
        slot.0
    }

    /// 释放一个逻辑键上的 watch，引用归零时删除记录。
    pub fn unwatch_key(&mut self, db: u8, key: &[u8]) {
        let entry = (namespace(db), key.to_vec());
        let Some(slot) = self.watched.get_mut(&entry)
        else {
            return;
        };
        slot.1 = slot.1.saturating_sub(1);
        if slot.1 == 0 {
            self.watched.remove(&entry);
        }
    }

    /// 被 watch 的逻辑键当前改动版本；未被 watch 时为 `0`。
    pub fn watch_version(&self, db: u8, key: &[u8]) -> u64 {
        self.watched.get(&(namespace(db), key.to_vec())).map(|slot| slot.0).unwrap_or(0)
    }

    /// 会话正在 watch 该逻辑键时递增它的版本。
    fn touch_watched(&mut self, key: &Key) {
        // 没有 watch 时热写路径不做任何版本记账。
        if self.watched.is_empty() {
            return;
        }
        let entry = (logical_namespace(&key.namespace.0), key.bytes.clone());
        if let Some(slot) = self.watched.get_mut(&entry) {
            slot.0 = slot.0.saturating_add(1);
        }
    }

    /// 选择每次快照写入之后是否跟随 `fsync`。
    ///
    /// `true` 是 `always` 策略；`false` 是 `everysec`，快照仍每次写入文件系统，
    /// 由 [`RedisShard::sync`] 把它变成断电稳定。
    pub fn set_sync_always(&mut self, always: bool) {
        self.sync_always = always;
    }

    /// 把未同步的快照刷到稳定存储。
    ///
    /// `everysec` 下宿主在周期定时器、`SAVE` / `BGSAVE` 与关停时调用。上次写入已同步时为空操作。
    pub fn sync(&mut self) -> io::Result<()> {
        if !self.unsynced {
            return Ok(());
        }
        let file = fs::OpenOptions::new().write(true).open(&self.shard_path)?;
        file.sync_all()?;
        self.unsynced = false;
        Ok(())
    }

    /// 有缓冲的计数或批次改动时写入分片快照。
    ///
    /// 计数更新留在内存直到这次调用、下一次其他改动、节点析构或进程正常退出。持久化边界
    /// 因此是一次完成的 `flush`：它之前写入的一切都能扛住崩溃，此后的增量会丢，重开的节点
    /// 报告上次 flush 时的值。快照本身仍经由临时文件与原子 rename 整份写入，崩溃永远不会
    /// 留下半截分片。
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.pending_increments && !self.dirty {
            return Ok(());
        }
        write_durable(&self.shard_path, &encode(&self.records, self.next_revision)?, self.sync_always)?;
        self.pending_increments = false;
        self.dirty = false;
        self.unsynced = !self.sync_always;
        Ok(())
    }

    fn bump_revision(&mut self) -> u64 {
        let revision = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        revision
    }

    /// 把 `changes` 应用到活映射，然后写入快照。
    ///
    /// 映射原地修改，只记住被触碰的记录，使编码或写入失败时可以放回原条目。这样避免了旧
    /// 实现每次提交都做的整份深拷贝。打开的批次内部，写盘推迟到 [`RedisShard::end_batch`]，
    /// 由它把整串改动一次编码。
    fn commit(&mut self, changes: Vec<Change>) -> io::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // 负截止时间是调用方错误，必须在任何改动之前失败。
        if changes.iter().any(|change| matches!(change, Change::Put { expires_at: Some(deadline), .. } if *deadline < 0)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expiry deadline must not be negative"));
        }
        let mut previous = Vec::with_capacity(changes.len());
        let mut next_revision = self.next_revision;
        for change in changes {
            match change {
                Change::Put { key, value, expires_at } => {
                    let revision = next_revision;
                    next_revision = next_revision.saturating_add(1);
                    let record = format::inline_record(key.clone(), value, revision, expires_at);
                    let old = self.records.insert(key.clone(), record);
                    self.touch_watched(&key);
                    previous.push((key, old));
                }
                Change::Delete { key } => {
                    let old = self.records.remove(&key);
                    if old.is_some() {
                        self.touch_watched(&key);
                    }
                    previous.push((key, old));
                }
            }
        }
        if self.batch_depth > 0 {
            self.next_revision = next_revision;
            // 缓冲的计数保持标记，使外层批次知道这次改动必须把它带进分片；`flush` 清除标记。
            self.dirty = true;
            return Ok(());
        }
        let encoded = match encode(&self.records, next_revision) {
            Ok(bytes) => bytes,
            Err(error) => {
                restore(&mut self.records, previous);
                return Err(error);
            }
        };
        if let Err(error) = write_durable(&self.shard_path, &encoded, self.sync_always) {
            restore(&mut self.records, previous);
            return Err(error);
        }
        self.next_revision = next_revision;
        // 刚写入的快照包含每个缓冲的增量。
        self.pending_increments = false;
        self.dirty = false;
        self.unsynced = !self.sync_always;
        Ok(())
    }
}

impl Drop for RedisShard {
    fn drop(&mut self) {
        let _ = self.flush();
        let _ = self.sync();
    }
}

/// 把失败提交之前捕获的条目放回，最新改动优先。
fn restore(records: &mut Records, previous: Vec<(Key, Option<Record>)>) {
    for (key, record) in previous.into_iter().rev() {
        match record {
            Some(record) => {
                records.insert(key, record);
            }
            None => {
                records.remove(&key);
            }
        }
    }
}

/// 既有 catalog 登记的分片文件缺失时，按登记创建该分片。
///
/// 这是 `catalog.shard-file` 打开错误的显式恢复入口：某个目录的 catalog 存活，但它登记的
/// `.yykv` 从未落盘，例如另一个工具写出、或只携带 `.yyds` 的备份被还原。catalog 被校验
/// 但从不重写，既有分片也从不被覆盖，因此在健康目录上调用不会改变任何东西。返回登记的分片 id。
pub fn repair_missing_shard(data_dir: impl AsRef<Path>) -> io::Result<String> {
    let data_dir = data_dir.as_ref().to_path_buf();
    let catalog_path = data_dir.join(CATALOG_FILE);
    let bytes = fs::read(&catalog_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("catalog.file: `{CATALOG_FILE}` is missing; open the directory normally to create it"),
            )
        }
        else {
            error
        }
    })?;
    let shard_id = decode_catalog_shard_id(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let shard_path = data_dir.join(format!("{shard_id}.yykv"));
    if !shard_path.exists() {
        write_durable(&shard_path, &encode(&Records::new(), 1)?, true)?;
    }
    Ok(shard_id)
}

/// 读取并校验既有 `.yyds`；文件缺失时为 `None`。
fn read_catalog(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => decode_catalog_shard_id(&bytes).map(Some).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

enum Change {
    Put { key: Key, value: Vec<u8>, expires_at: Option<i64> },
    Delete { key: Key },
}

/// 一个 Redis 数据库序号对应的命名空间标签。
pub fn namespace(db: u8) -> String {
    format!("{NAMESPACE_PREFIX}{db}")
}

/// 数据库内某一种值种类的命名空间标签。
///
/// 字符串种类沿用数据库命名空间，使既有数据无需迁移，而 `hash`、`list`、`set`、`zset`
/// 与 `stream` 各自追加后缀。六个命名空间互不重叠，因此同一个逻辑键在任一时刻只能被
/// 读成一种值种类。
pub fn kind_namespace(db: u8, kind: &str) -> String {
    if kind.is_empty() { namespace(db) } else { format!("{}:{kind}", namespace(db)) }
}

/// 分片里一个逻辑键某一种值种类的键。
fn kind_key(db: u8, kind: &str, key: &[u8]) -> Key {
    Key::new(kind_namespace(db, kind), key.to_vec())
}

/// 记录命名空间背后的数据库命名空间，去掉任何种类后缀。
fn logical_namespace(candidate: &str) -> String {
    if let Some(rest) = candidate.strip_prefix(NAMESPACE_PREFIX) {
        if let Some(index) = rest.find(':') {
            return format!("{NAMESPACE_PREFIX}{}", &rest[..index]);
        }
    }
    candidate.to_owned()
}

/// 记录命名空间里编码的数据库序号；不是分片命名空间时为 `None`。
fn database_index(candidate: &str) -> Option<u8> {
    let rest = candidate.strip_prefix(NAMESPACE_PREFIX)?;
    let index = rest.split_once(':').map_or(rest, |(index, _)| index);
    index.parse().ok()
}

/// 记录命名空间是否属于 `base` 命名的数据库。
///
/// 比较整个尾段，因此 `redis:db1` 永不匹配 `redis:db11`。
fn belongs_to(candidate: &str, base: &str) -> bool {
    candidate == base || candidate.strip_prefix(base).is_some_and(|rest| rest.starts_with(':'))
}

fn encode(records: &Records, revision: u64) -> io::Result<Vec<u8>> {
    format::encode_shard(records, revision).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// 经由已同步的临时文件与原子 rename 把 `bytes` 写到 `path`。
fn write_durable(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        if sync {
            file.sync_all()?;
        }
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

/// 为测试与导入构建记录映射。
pub fn records_from_entries(entries: impl IntoIterator<Item = (u8, Vec<u8>, Vec<u8>, Option<i64>)>) -> Records {
    let mut records = BTreeMap::new();
    for (index, key, value, expires_at) in entries {
        let record_key = Key::new(namespace(index), key);
        records.insert(record_key.clone(), format::inline_record(record_key, value, 1, expires_at));
    }
    records
}
