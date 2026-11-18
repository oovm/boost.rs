//! Redis 的字符串与键空间命令族：解析后的命令语义、参数校验与过期判定。
//!
//! `GET` / `SET` / 计数器 / 位域 / 过期 / 键搬移 / 枚举 / 库级操作共享同一条执行链。
//! 命令只决定「对哪个命名空间做什么」，值的读写都经过 [`KeyValueExecutor`]，类型编码
//! 在命名空间上，因此本模块不持有存储，也不碰套接字：帧读写、连接登记、事务队列与订阅
//! 路由留在服务端连接层。
//!
//! `OBJECT IDLETIME` 的空闲计时是真机按对象记录的命令语义，随命令族留在本模块；计时器
//! 由监听器创建并跨连接共享，进程内保存、重启归零。

use std::{
    collections::HashMap,
    io::{self, Write},
    sync::Mutex,
    time::Instant,
};

use database_protocol_redis::{
    bind::{BindError, BoundAction, BoundCommand, SetCondition, SetExpiry, bind, bind_mget, bind_set},
    resp::Request,
};
use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};
use database_storage::cluster_kv::{
    now_unix_ms,
    redis::format::{decode_hash, decode_list, decode_set, decode_zset},
};
use database_types::cluster::{FenceToken, Namespace};

use super::{
    KeyValueExecutor,
    collections::{self, KeyKind},
    reply::{glob_match, write_bulk, write_error, write_null, write_unknown_command},
    session::{parse_database, parse_integer},
};

/// 协议绑定结果 → 执行层键值命令。
fn to_kv_command(command: BoundCommand) -> KeyValueCommand {
    KeyValueCommand {
        namespace: Namespace(command.namespace),
        key: command.key,
        action: match command.action {
            BoundAction::Get => KeyValueAction::Get,
            BoundAction::Exists => KeyValueAction::Exists,
            BoundAction::GetDelete => KeyValueAction::GetDelete,
            BoundAction::GetSet(value) => KeyValueAction::GetSet(value),
            BoundAction::Put(value) => KeyValueAction::Put(value),
            BoundAction::PutIfAbsent(value) => KeyValueAction::PutIfAbsent(value),
            BoundAction::PutIfPresent(value) => KeyValueAction::PutIfPresent(value),
            BoundAction::Delete => KeyValueAction::Delete,
            BoundAction::IncrementBy(delta) => KeyValueAction::IncrementBy(delta),
        },
    }
}

/// 进程内键访问计时器，供 `OBJECT IDLETIME` 使用。
///
/// 只保存每个键「最近一次命中」的单调时钟时刻，不写入 `.yykv`：`Record` 与 v3 落盘
/// 格式都不带访问元数据，塞进记录会改变落盘字节并破坏格式兼容。计时器因此随监听器
/// 进程存在，重启后归零——这与真机一致，Redis 的 RDB 同样不保存访问时间，加载后对象
/// 从零重新计时。
#[derive(Debug, Default)]
pub struct KeyAccessTracker {
    last_access: Mutex<HashMap<(Namespace, Vec<u8>), Instant>>,
}

impl KeyAccessTracker {
    /// 创建一个空计时器，通常由监听器启动时创建并在各连接间共享。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次命中，把该键的空闲起点推进到当前时刻。
    fn mark(&self, namespace: &Namespace, key: &[u8]) {
        let mut entries = self.last_access.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert((namespace.clone(), key.to_vec()), Instant::now());
    }

    /// 键不再可见时丢弃记录，避免同名键重新创建后读出上一轮的空闲时长。
    fn forget(&self, namespace: &Namespace, key: &[u8]) {
        let mut entries = self.last_access.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.remove(&(namespace.clone(), key.to_vec()));
    }

    /// 空闲秒数。没有记录的键按「刚刚加载」处理并返回 `0`，对应重启后的起始状态。
    fn idle_seconds(&self, namespace: &Namespace, key: &[u8]) -> u64 {
        let entries = self.last_access.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.get(&(namespace.clone(), key.to_vec())).map_or(0, |last| last.elapsed().as_secs())
    }
}

/// 包装执行器，在每条键值命令返回后更新访问计时。
///
/// 读命中与写命中统一在这里刷新空闲起点，不必在每个协议分支里各写一次 `mark`。键变为
/// 不可见时丢弃记录。`OBJECT` 子命令改用未包装的执行器查键，因此保持真机的
/// `LOOKUP_NOTOUCH` 语义：查询空闲时间不会把空闲时间清零。
#[derive(Debug)]
pub struct TrackedExecutor<'a> {
    inner: &'a dyn KeyValueExecutor,
    tracker: &'a KeyAccessTracker,
}

impl<'a> TrackedExecutor<'a> {
    /// 包装一个执行器，用共享计时器记录它返回的每次命中。
    pub fn new(inner: &'a dyn KeyValueExecutor, tracker: &'a KeyAccessTracker) -> Self {
        Self { inner, tracker }
    }

    /// 依据一次键值命令的结果更新计时。执行失败时保持原记录，不猜测存储状态。
    fn observe(&self, namespace: &Namespace, key: &[u8], destination: Option<&[u8]>, result: &KeyValueResult) {
        match result {
            KeyValueResult::Get(Some(_))
            | KeyValueResult::GetSet(_)
            | KeyValueResult::Put { .. }
            // `PutIfAbsent` 返回 `None` 表示旧键仍在，条件写失败本身也是一次命中。
            | KeyValueResult::PutIfAbsent { .. }
            | KeyValueResult::Increment { .. }
            | KeyValueResult::Exists(true) => self.tracker.mark(namespace, key),
            KeyValueResult::PutIfPresent { revision: Some(_) } => self.tracker.mark(namespace, key),
            KeyValueResult::Get(None)
            | KeyValueResult::Exists(false)
            | KeyValueResult::GetDelete(_)
            | KeyValueResult::Delete { .. }
            | KeyValueResult::PutIfPresent { revision: None } => self.tracker.forget(namespace, key),
            // 修订号读取是纯版本探测（`WATCH`/`EXEC` 用），不刷新空闲计时，与 `OBJECT` 同理。
            KeyValueResult::Revision(_) => {}
            KeyValueResult::Expiry { present, .. } => {
                if *present {
                    self.tracker.mark(namespace, key)
                }
                else {
                    self.tracker.forget(namespace, key)
                }
            }
            KeyValueResult::ExpiryChanged { applied } => {
                // 未生效时无法区分「键不存在」与「本来就没有过期时间」，保留原记录。
                if *applied {
                    self.tracker.mark(namespace, key)
                }
            }
            KeyValueResult::Rename { source_present, applied } => {
                if *applied {
                    self.tracker.forget(namespace, key);
                    if let Some(destination) = destination {
                        self.tracker.mark(namespace, destination);
                    }
                }
                else if *source_present {
                    self.tracker.mark(namespace, key);
                }
                else {
                    self.tracker.forget(namespace, key);
                }
            }
        }
    }
}

impl KeyValueExecutor for TrackedExecutor<'_> {
    fn execute(&self, fence: Option<&FenceToken>, command: KeyValueCommand) -> database_types::cluster::Result<KeyValueResult> {
        // 命令会被移交给内层执行器，计时所需字段先复制出来。
        let namespace = command.namespace.clone();
        let key = command.key.clone();
        let destination = match &command.action {
            KeyValueAction::Rename { destination, .. } => Some(destination.clone()),
            _ => None,
        };
        let result = self.inner.execute(fence, command);
        if let Ok(value) = &result {
            self.observe(&namespace, &key, destination.as_deref(), value);
        }
        result
    }

    fn execute_read_batch(
        &self,
        fence: Option<&FenceToken>,
        commands: &[KeyValueCommand],
    ) -> database_types::cluster::Result<Vec<KeyValueResult>> {
        let result = self.inner.execute_read_batch(fence, commands);
        if let Ok(results) = &result {
            for (command, value) in commands.iter().zip(results) {
                self.observe(&command.namespace, &command.key, None, value);
            }
        }
        result
    }

    fn execute_scan(
        &self,
        fence: Option<&FenceToken>,
        namespace: &Namespace,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> database_types::cluster::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // 扫描不指向单个键，保持原样透传。
        self.inner.execute_scan(fence, namespace, prefix, after, before, limit)
    }
}

/// Writes the Redis-shaped reply for a rejected `SET` option list.
fn write_set_error(stream: &mut dyn Write, error: BindError) -> io::Result<()> {
    match error {
        BindError::WrongArity => stream.write_all(b"-ERR wrong number of arguments for 'set' command\r\n"),
        BindError::Syntax => stream.write_all(b"-ERR syntax error\r\n"),
        BindError::InvalidInteger => stream.write_all(b"-ERR value is not an integer or out of range\r\n"),
        BindError::InvalidExpire => stream.write_all(b"-ERR invalid expire time in 'set' command\r\n"),
        BindError::UnsupportedOptions => stream.write_all(b"-ERR syntax error\r\n"),
        BindError::EmptyCommand => stream.write_all(b"-ERR empty command\r\n"),
        BindError::UnsupportedCommand => stream.write_all(b"-ERR unsupported command\r\n"),
    }
}

/// Executes `SET key value [NX|XX] [EX <s>|PX <ms>] [KEEPTTL]`.
///
/// The write goes through [`KeyValueExecutor`] with one expiry-aware action per Redis
/// option matrix, so the value and its deadline are applied in a single executor step:
/// an explicit `EX` / `PX` stores the deadline, `KEEPTTL` preserves the deadline of the
/// replaced record and a plain overwrite clears it. A failed `NX` / `XX` neither writes
/// the value nor touches any deadline.
fn handle_set(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<()> {
    let Some(executor) = executor
    else {
        stream.write_all(b"-ERR unsupported command\r\n")?;
        return Ok(());
    };
    let parsed = match bind_set(&request.arguments) {
        Ok(parsed) => parsed,
        Err(error) => return write_set_error(stream, error),
    };
    // SET 会无条件替换键，因此条件判断必须看到集合类型：`NX` 在 hash 上要回 nil，
    // `XX` 在 hash 上要照常替换。字符串命名空间为空时 `PutIfPresent` 会误报未命中，
    // 所以非字符串上的 XX 改走无条件写入。
    let existing = match resolve_kind(stream, executor, namespace, &parsed.key)? {
        Resolved::Rejected => return Ok(()),
        Resolved::Missing => None,
        Resolved::Kind(kind) => Some(kind),
    };
    // `SET ... GET` 先取旧值：真机在读不到字符串（键为集合）时直接 WRONGTYPE，
    // 不执行写入，也不再看 NX / XX 条件。
    let old_value = if parsed.get {
        match existing {
            Some(KeyKind::String) => match load_string(executor, namespace, &parsed.key) {
                Ok(value) => value,
                Err(message) => {
                    write_error(stream, &message)?;
                    return Ok(());
                }
            },
            Some(_) => {
                collections::write_wrong_type(stream)?;
                return Ok(());
            }
            None => None,
        }
    }
    else {
        None
    };
    let condition = match parsed.condition {
        SetCondition::IfAbsent if existing.is_some() => return write_set_reply(stream, protocol, parsed.get, &old_value, false),
        SetCondition::IfPresent if existing.is_none() => return write_set_reply(stream, protocol, parsed.get, &old_value, false),
        SetCondition::IfPresent if !matches!(existing, Some(KeyKind::String)) => SetCondition::Always,
        condition => condition,
    };
    // `KEEPTTL` 要继承原键的过期时间；字符串由执行器自己保留，集合键需要先读出来再补写。
    let inherited_expiry = match existing {
        Some(kind) if parsed.keep_ttl && kind != KeyKind::String => {
            let command = database_query::cluster_kv::KeyValueCommand {
                namespace: kind.namespace(namespace),
                key: parsed.key.clone(),
                action: database_query::cluster_kv::KeyValueAction::GetExpiry,
            };
            match executor.execute(None, command) {
                Ok(KeyValueResult::Expiry { present: true, expires_at }) => expires_at,
                _ => None,
            }
        }
        _ => None,
    };
    let action = match (condition, parsed.expiry) {
        (SetCondition::Always, Some(expiry)) => match set_deadline(expiry) {
            Some(expires_at) => database_query::cluster_kv::KeyValueAction::PutExpiring { value: parsed.value, expires_at },
            None => return write_set_error(stream, BindError::InvalidExpire),
        },
        // NX on an absent key has no existing deadline to keep, so plain PutIfAbsent is exact.
        (SetCondition::IfAbsent, Some(expiry)) => match set_deadline(expiry) {
            Some(expires_at) => database_query::cluster_kv::KeyValueAction::PutIfAbsentExpiring { value: parsed.value, expires_at },
            None => return write_set_error(stream, BindError::InvalidExpire),
        },
        (SetCondition::IfAbsent, None) => database_query::cluster_kv::KeyValueAction::PutIfAbsent(parsed.value),
        (SetCondition::IfPresent, Some(expiry)) => match set_deadline(expiry) {
            Some(expires_at) => database_query::cluster_kv::KeyValueAction::PutIfPresentExpiring { value: parsed.value, expires_at },
            None => return write_set_error(stream, BindError::InvalidExpire),
        },
        (SetCondition::IfPresent, None) if parsed.keep_ttl => {
            database_query::cluster_kv::KeyValueAction::PutIfPresentKeepingExpiry(parsed.value)
        }
        (SetCondition::IfPresent, None) => database_query::cluster_kv::KeyValueAction::PutIfPresent(parsed.value),
        (SetCondition::Always, None) if parsed.keep_ttl => database_query::cluster_kv::KeyValueAction::PutKeepingExpiry(parsed.value),
        (SetCondition::Always, None) => database_query::cluster_kv::KeyValueAction::Put(parsed.value),
    };
    let result =
        executor.execute(None, database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: parsed.key.clone(), action });
    let stored = match result {
        Ok(KeyValueResult::Put { .. }) => true,
        Ok(KeyValueResult::PutIfAbsent { revision }) | Ok(KeyValueResult::PutIfPresent { revision }) => revision.is_some(),
        Ok(_) => {
            write_error(stream, "SET executor returned unexpected result")?;
            return Ok(());
        }
        Err(error) => {
            write_error(stream, &error.to_string())?;
            return Ok(());
        }
    };
    if !stored {
        // The condition failed: no write happened, so any existing deadline survives.
        return write_set_reply(stream, protocol, parsed.get, &old_value, false);
    }
    if let Some(expires_at) = inherited_expiry {
        let action = database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at };
        let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: parsed.key.clone(), action };
        if let Err(error) = executor.execute(None, command) {
            write_error(stream, &error.to_string())?;
            return Ok(());
        }
    }
    // 覆盖写成功后清掉旧的集合表示，保证一个逻辑键只有一种类型。
    if let Err(message) = clear_other_kinds(executor, namespace, &parsed.key) {
        write_error(stream, &message)?;
        return Ok(());
    }
    write_set_reply(stream, protocol, parsed.get, &old_value, true)
}

/// `SET` 的回复：带 `GET` 时回旧值（缺失为 nil），否则成功回 `+OK`、条件不满足回 nil。
fn write_set_reply(stream: &mut dyn Write, protocol: u8, get: bool, old_value: &Option<Vec<u8>>, success: bool) -> io::Result<()> {
    if get {
        return match old_value {
            Some(value) => write_bulk(stream, value),
            None => write_null(stream, protocol),
        };
    }
    if success { stream.write_all(b"+OK\r\n") } else { write_null(stream, protocol) }
}

/// Absolute Unix-millisecond deadline for a SET expiry option, unless it overflows.
///
/// `EX` / `PX` are offsets from the server's current time; `EXAT` / `PXAT`
/// name the deadline directly (records carry wall-clock deadlines).
fn set_deadline(expiry: SetExpiry) -> Option<i64> {
    match expiry {
        SetExpiry::Seconds(seconds) => now_unix_ms().checked_add(i64::try_from(seconds.checked_mul(1000)?).ok()?),
        SetExpiry::Milliseconds(milliseconds) => now_unix_ms().checked_add(i64::try_from(milliseconds).ok()?),
        SetExpiry::AtSeconds(seconds) => i64::try_from(seconds).ok()?.checked_mul(1000),
        SetExpiry::AtMilliseconds(milliseconds) => i64::try_from(milliseconds).ok(),
    }
}

/// 跨类型探测一个逻辑键的结果。
///
/// `Rejected` 表示错误回复已经写入连接，调用方不得再追加字节。
enum Resolved {
    Missing,
    Kind(KeyKind),
    Rejected,
}

/// 探测逻辑键当前类型；探测失败时写成 RESP 错误并返回 `Rejected`。
fn resolve_kind(stream: &mut dyn Write, executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> io::Result<Resolved> {
    match collections::find_kind(executor, namespace, key) {
        Ok(Some(kind)) => Ok(Resolved::Kind(kind)),
        Ok(None) => Ok(Resolved::Missing),
        Err(message) => {
            write_error(stream, &message)?;
            Ok(Resolved::Rejected)
        }
    }
}

/// 删除同一逻辑键的其它类型表示。
///
/// `SET` / `SETEX` 会无条件替换键，而 Redis 的键一次只承载一种类型；写完字符串后必须
/// 清掉 hash/list 表示，否则 `TYPE`、`EXISTS` 与后续写命令会看到两个并存的值。
fn clear_other_kinds(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<(), String> {
    for kind in KeyKind::ALL {
        if kind == KeyKind::String {
            continue;
        }
        let command = KeyValueCommand { namespace: kind.namespace(namespace), key: key.to_vec(), action: KeyValueAction::Delete };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Delete { .. }) => {}
            Ok(_) => return Err("delete returned an unexpected result".into()),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

/// 断言逻辑键当前是字符串或不存在；失败时错误已写入，返回 `false`。
fn ensure_string(stream: &mut dyn Write, executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> io::Result<bool> {
    match resolve_kind(stream, executor, namespace, key)? {
        Resolved::Kind(KeyKind::String) | Resolved::Missing => Ok(true),
        Resolved::Kind(_) => {
            collections::write_wrong_type(stream)?;
            Ok(false)
        }
        Resolved::Rejected => Ok(false),
    }
}

/// 读取字符串键的原始值；缺键回 `None`。
fn load_string(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
    match executor.execute(None, KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::Get }) {
        Ok(KeyValueResult::Get(value)) => Ok(value),
        Ok(_) => Err("GET executor returned unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 写入字符串键并保留原有过期时间（缺键即普通写入）。
fn store_string(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8], value: Vec<u8>) -> Result<(), String> {
    match executor
        .execute(None, KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::PutKeepingExpiry(value) })
    {
        Ok(KeyValueResult::Put { .. }) => Ok(()),
        Ok(_) => Err("PUT executor returned unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 解析无符号偏移：只接受规范十进制写法（拒绝空串、符号与带前导零的写法）。
fn parse_offset(raw: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(raw).ok()?;
    let value = text.parse::<u64>().ok()?;
    if value.to_string() != text { None } else { Some(value) }
}

/// Redis 字符串上限 512MB，位偏移上限为 2^32-1。
const MAX_STRING_BYTES: u64 = 512 * 1024 * 1024;

/// 把闭区间 `[start, stop]` 归一化为可安全切片的下标；空区间回 `None`。
fn normalized_range(start: i64, stop: i64, length: usize) -> Option<(usize, usize)> {
    let length = length as i64;
    let begin = if start < 0 { length.saturating_add(start) } else { start }.max(0);
    let end = if stop < 0 { length.saturating_add(stop) } else { stop }.min(length - 1);
    if begin > end || begin >= length || end < 0 { None } else { Some((begin as usize, end as usize)) }
}

/// 把 `BITPOS` 的字节或位索引归一化到 `[0, units - 1]`：负索引从尾部起算，越界被裁剪。
///
/// 归一化后的区间仍可能为空（例如空字符串得到 `units == 0`），由调用方判定 `-1`。
fn normalize_unit(index: i64, units: i64) -> i64 {
    let normalized = if index < 0 { units.saturating_add(index) } else { index };
    normalized.clamp(0, (units - 1).max(0))
}

/// 统计一段 bit 区间（含端点）中被置位的数量。
fn count_set_bits(value: &[u8], start: usize, end: usize) -> i64 {
    let mut total = 0_i64;
    for bit in start..=end {
        if value[bit / 8] & (1 << (7 - bit % 8)) != 0 {
            total += 1;
        }
    }
    total
}

/// 定位 `[start, end]` 内第一个等于 `target` 的位；找不到回 `None`。
fn find_bit(value: &[u8], start: usize, end: usize, target: u8) -> Option<usize> {
    for bit in start..=end {
        let set = value[bit / 8] & (1 << (7 - bit % 8)) != 0;
        if set == (target == 1) {
            return Some(bit);
        }
    }
    None
}

/// `INCRBYFLOAT` 的回复与存储格式：最短十进制表示，整数不带 `.0`。
fn format_float(value: f64) -> String {
    value.to_string()
}

/// `EXPIRE` 家族尾部的条件选项。
#[derive(Debug, Clone, Copy, Default)]
struct ExpireFlags {
    /// 仅当键当前没有过期时间时生效。
    nx: bool,
    /// 仅当键当前已有过期时间时生效。
    xx: bool,
    /// 仅当新截止时间大于当前截止时间时生效。
    gt: bool,
    /// 仅当新截止时间小于当前截止时间时生效；无过期时间视为无限大。
    lt: bool,
}

impl ExpireFlags {
    /// 是否带了任意一个条件。
    fn any(self) -> bool {
        self.nx || self.xx || self.gt || self.lt
    }
}

/// 条件选项解析失败的原因。
enum ExpireFlagError {
    /// `NX` 与 `XX` / `GT` / `LT` 冲突。
    Conflict,
    /// `GT` 与 `LT` 冲突，真机使用另一句错误文案。
    GtLtConflict,
    /// 不认识的选项原文。
    Unsupported(Vec<u8>),
}

/// 解析 `EXPIRE key seconds [NX|XX|GT|LT ...]` 中第二个参数之后的条件选项。
///
/// 与真机一致：选项大小写不敏感、允许重复，但 `NX` 不能和 `XX` / `GT` / `LT`
/// 同时出现，`GT` 与 `LT` 也不能同时出现。
fn parse_expire_flags(arguments: &[&[u8]]) -> Result<ExpireFlags, ExpireFlagError> {
    let mut flags = ExpireFlags::default();
    for raw in &arguments[3..] {
        if raw.eq_ignore_ascii_case(b"NX") {
            flags.nx = true;
        }
        else if raw.eq_ignore_ascii_case(b"XX") {
            flags.xx = true;
        }
        else if raw.eq_ignore_ascii_case(b"GT") {
            flags.gt = true;
        }
        else if raw.eq_ignore_ascii_case(b"LT") {
            flags.lt = true;
        }
        else {
            return Err(ExpireFlagError::Unsupported(raw.to_vec()));
        }
    }
    if flags.nx && (flags.xx || flags.gt || flags.lt) {
        return Err(ExpireFlagError::Conflict);
    }
    if flags.gt && flags.lt {
        return Err(ExpireFlagError::GtLtConflict);
    }
    Ok(flags)
}

/// 把条件选项的错误写回客户端，消息与真机逐字一致。
fn write_expire_flag_error(stream: &mut dyn Write, error: ExpireFlagError) -> io::Result<()> {
    match error {
        ExpireFlagError::Conflict => write_error(stream, "NX and XX, GT or LT options at the same time are not compatible"),
        ExpireFlagError::GtLtConflict => write_error(stream, "GT and LT options at the same time are not compatible"),
        ExpireFlagError::Unsupported(option) => {
            let text = String::from_utf8_lossy(&option);
            write_error(stream, &format!("Unsupported option {text}"))
        }
    }
}

/// 判定条件选项是否允许把截止时间设为 `expires_at`；`Ok(None)` 表示错误已经写出。
///
/// 真机的条件是相对「当前截止时间」比较：`NX` 要求当前无过期，`XX` 要求当前有过期，
/// `GT` 要求严格更大，`LT` 要求严格更小，而当前无过期时 `LT` 视为无限大所以总成立。
fn expire_condition_allows(
    stream: &mut dyn Write,
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    key: &[u8],
    flags: ExpireFlags,
    expires_at: i64,
) -> io::Result<Option<bool>> {
    if !flags.any() {
        return Ok(Some(true));
    }
    let current =
        match executor.execute(None, KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::GetExpiry }) {
            Ok(KeyValueResult::Expiry { expires_at, .. }) => expires_at,
            Ok(_) => {
                write_error(stream, "EXPIRE executor returned unexpected result")?;
                return Ok(None);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(None);
            }
        };
    let allowed = if flags.nx {
        current.is_none()
    }
    else if flags.xx {
        current.is_some()
    }
    else if flags.gt {
        current.is_some_and(|current| expires_at > current)
    }
    else {
        current.map_or(true, |current| expires_at < current)
    };
    Ok(Some(allowed))
}

/// `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]`。
///
/// 游标是「已检查键数」的十进制下标：每次返回 `COUNT` 个键（不足则到末尾），返回 0
/// 表示迭代结束。所有值类型都参与扫描，键按字节序合并后去重，因此一次调用不保证
/// 返回全部键，但持续回传游标直到 0 能覆盖当前键空间。`TYPE` 只筛出该类型的键；
/// 真机对未知类型名不回错误，只是没有任何键能命中，这里同样返回空结果。
fn handle_scan(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
) -> io::Result<()> {
    let Some(executor) = executor
    else {
        stream.write_all(b"-ERR unsupported command\r\n")?;
        return Ok(());
    };
    if request.arguments.len() < 2 {
        stream.write_all(b"-ERR wrong number of arguments for 'scan' command\r\n")?;
        return Ok(());
    }
    let cursor_text = match std::str::from_utf8(request.arguments[1]) {
        Ok(text) if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => text,
        _ => {
            write_error(stream, "invalid cursor")?;
            return Ok(());
        }
    };
    let Ok(cursor) = cursor_text.parse::<u64>()
    else {
        write_error(stream, "invalid cursor")?;
        return Ok(());
    };
    let mut pattern: Option<Vec<u8>> = None;
    let mut kind_filter: Option<Option<KeyKind>> = None;
    let mut count = 10usize;
    let mut index = 2;
    while index < request.arguments.len() {
        if index + 1 >= request.arguments.len() {
            write_error(stream, "syntax error")?;
            return Ok(());
        }
        if request.arguments[index].eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(request.arguments[index + 1].to_vec());
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"COUNT") {
            // 真机把越界的 COUNT 报成整数错误，把非正数报成语法错误，两者不能混用。
            let Ok(value) = std::str::from_utf8(request.arguments[index + 1]).unwrap_or("").parse::<i64>()
            else {
                write_error(stream, "value is not an integer or out of range")?;
                return Ok(());
            };
            if value <= 0 {
                write_error(stream, "syntax error")?;
                return Ok(());
            }
            count = usize::try_from(value).unwrap_or(usize::MAX);
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"TYPE") {
            // 未知类型名在真机上不是错误，只是筛不出任何键，所以这里保留 `None` 并继续过滤。
            kind_filter = Some(KeyKind::from_name(request.arguments[index + 1]));
        }
        else {
            write_error(stream, "syntax error")?;
            return Ok(());
        }
        index += 2;
    }
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for kind in KeyKind::ALL {
        if let Some(filter) = kind_filter {
            if filter != Some(kind) {
                continue;
            }
        }
        match executor.execute_scan(None, &kind.namespace(namespace), &[], None, None, 0) {
            Ok(records) => keys.extend(records.into_iter().map(|(key, _)| key)),
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(());
            }
        }
    }
    keys.sort();
    keys.dedup();
    let start = cursor as usize;
    if start >= keys.len() {
        stream.write_all(b"*2\r\n")?;
        write_bulk(stream, b"0")?;
        stream.write_all(b"*0\r\n")?;
        return Ok(());
    }
    let end = start.saturating_add(count).min(keys.len());
    let next = if end >= keys.len() { "0".to_string() } else { end.to_string() };
    let matched: Vec<&Vec<u8>> =
        keys[start..end].iter().filter(|key| pattern.as_ref().map_or(true, |pattern| glob_match(pattern, key))).collect();
    stream.write_all(b"*2\r\n")?;
    write_bulk(stream, next.as_bytes())?;
    write!(stream, "*{}\r\n", matched.len())?;
    for key in matched {
        write_bulk(stream, key)?;
    }
    Ok(())
}

/// 收集一个逻辑库下所有现存键，按字节序去重。
fn collect_keys(executor: &dyn KeyValueExecutor, base: &Namespace) -> Result<Vec<Vec<u8>>, String> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for kind in KeyKind::ALL {
        match executor.execute_scan(None, &kind.namespace(base), &[], None, None, 0) {
            Ok(records) => keys.extend(records.into_iter().map(|(key, _)| key)),
            Err(error) => return Err(error.to_string()),
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// Builds the base namespace of another logical database from the current one.
///
/// The L1 gateway names databases `redis:<index>` while an embedded binding may
/// mount the same catalog under a prefix of its own, so only the trailing index
/// is replaced and the prefix is preserved.
pub fn database_namespace(base: &Namespace, database: u32) -> Namespace {
    let text = base.0.as_str();
    let split = text.rfind(|character: char| !character.is_ascii_digit()).map_or(0, |index| index + 1);
    Namespace(format!("{}{database}", &text[..split]))
}

/// 清空一组逻辑库的全部类型命名空间，返回第一条失败信息。
fn flush_databases(executor: &dyn KeyValueExecutor, databases: impl IntoIterator<Item = Namespace>) -> Option<String> {
    for base in databases {
        for kind in KeyKind::ALL {
            let target = kind.namespace(&base);
            match executor.execute_scan(None, &target, &[], None, None, 0) {
                Ok(records) => {
                    for (key, _) in records {
                        let command = KeyValueCommand { namespace: target.clone(), key, action: KeyValueAction::Delete };
                        if let Err(error) = executor.execute(None, command) {
                            return Some(error.to_string());
                        }
                    }
                }
                Err(error) => return Some(error.to_string()),
            }
        }
    }
    None
}

/// 进程内伪随机下标，`RANDOMKEY` 用它挑一个键。
fn random_index(bound: usize) -> usize {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_nanos() as u64).unwrap_or(1);
    let mut value = nanos ^ 0x9E37_79B9_7F4A_7C15;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    (value as usize) % bound
}

/// `COPY source destination [DB destination-db] [REPLACE]`。
///
/// 按源键的实际类型复制值 blob，目标键先清空全部类型表示再写入；源键的剩余过期时间
/// 随副本一起复制，与真机一致。
fn handle_copy(stream: &mut dyn Write, request: &Request<'_>, executor: &dyn KeyValueExecutor, namespace: &Namespace) -> io::Result<()> {
    if request.arguments.len() < 3 {
        return write_error(stream, "wrong number of arguments for 'copy' command");
    }
    let source = request.arguments[1];
    let destination = request.arguments[2];
    let mut destination_database = None;
    let mut replace = false;
    let mut index = 3;
    while index < request.arguments.len() {
        if request.arguments[index].eq_ignore_ascii_case(b"DB") && index + 1 < request.arguments.len() {
            let Some(database) = parse_database(request.arguments[index + 1])
            else {
                // 真机先按整数解析再判范围：非整数报值错误，越界（含负数与溢出）报范围错误。
                let message = if parse_integer(request.arguments[index + 1]).is_some() {
                    "DB index is out of range"
                }
                else {
                    "value is not an integer or out of range"
                };
                return write_error(stream, message);
            };
            destination_database = Some(database);
            index += 2;
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"REPLACE") {
            replace = true;
            index += 1;
        }
        else {
            return write_error(stream, "syntax error");
        }
    }
    let destination_base = match destination_database {
        Some(database) => database_namespace(namespace, u32::from(database)),
        None => namespace.clone(),
    };
    if destination_base == *namespace && source == destination {
        return write_error(stream, "source and destination objects are the same");
    }
    let source_kind = match collections::find_kind(executor, namespace, source) {
        Ok(Some(kind)) => kind,
        Ok(None) => {
            stream.write_all(b":0\r\n")?;
            return Ok(());
        }
        Err(message) => return write_error(stream, &message),
    };
    let destination_exists = match collections::find_kind(executor, &destination_base, destination) {
        Ok(exists) => exists.is_some(),
        Err(message) => return write_error(stream, &message),
    };
    if destination_exists && !replace {
        stream.write_all(b":0\r\n")?;
        return Ok(());
    }
    let source_namespace = source_kind.namespace(namespace);
    let value = match executor
        .execute(None, KeyValueCommand { namespace: source_namespace.clone(), key: source.to_vec(), action: KeyValueAction::Get })
    {
        Ok(KeyValueResult::Get(Some(value))) => value,
        Ok(KeyValueResult::Get(None)) => {
            stream.write_all(b":0\r\n")?;
            return Ok(());
        }
        Ok(_) => return write_error(stream, "COPY executor returned unexpected result"),
        Err(error) => return write_error(stream, &error.to_string()),
    };
    let expires_at = match executor
        .execute(None, KeyValueCommand { namespace: source_namespace, key: source.to_vec(), action: KeyValueAction::GetExpiry })
    {
        Ok(KeyValueResult::Expiry { expires_at, .. }) => expires_at,
        Ok(_) => return write_error(stream, "COPY executor returned unexpected result"),
        Err(error) => return write_error(stream, &error.to_string()),
    };
    for kind in KeyKind::ALL {
        let command =
            KeyValueCommand { namespace: kind.namespace(&destination_base), key: destination.to_vec(), action: KeyValueAction::Delete };
        if let Err(error) = executor.execute(None, command) {
            return write_error(stream, &error.to_string());
        }
    }
    let destination_namespace = source_kind.namespace(&destination_base);
    match executor.execute(
        None,
        KeyValueCommand { namespace: destination_namespace.clone(), key: destination.to_vec(), action: KeyValueAction::Put(value) },
    ) {
        Ok(KeyValueResult::Put { .. }) => {}
        Ok(_) => return write_error(stream, "COPY executor returned unexpected result"),
        Err(error) => return write_error(stream, &error.to_string()),
    }
    if let Some(expires_at) = expires_at {
        let command =
            KeyValueCommand { namespace: destination_namespace, key: destination.to_vec(), action: KeyValueAction::SetExpiry { expires_at } };
        if let Err(error) = executor.execute(None, command) {
            return write_error(stream, &error.to_string());
        }
    }
    stream.write_all(b":1\r\n")
}

/// `OBJECT IDLETIME` / `OBJECT REFCOUNT` / `OBJECT FREQ`.
///
/// 三个子命令先按真机的 `LOOKUP_NOTOUCH` 口径查键：缺键回 null（RESP2 `$-1`、RESP3 `_`），
/// 查询本身不刷新访问计时。这里用未包装的执行器，因此 `OBJECT IDLETIME` 不会把空闲
/// 时间清零。
fn handle_object(
    stream: &mut dyn Write,
    request: &Request<'_>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<()> {
    let count = request.arguments.len();
    let subcommand = request.arguments.get(1).copied().unwrap_or(b"");
    if count < 2 {
        stream.write_all(b"-ERR wrong number of arguments for 'object' command\r\n")?;
        return Ok(());
    }
    let known = subcommand.eq_ignore_ascii_case(b"IDLETIME")
        || subcommand.eq_ignore_ascii_case(b"REFCOUNT")
        || subcommand.eq_ignore_ascii_case(b"FREQ")
        || subcommand.eq_ignore_ascii_case(b"ENCODING");
    if !known {
        write_error(stream, &format!("unknown subcommand '{}'. Try OBJECT HELP.", String::from_utf8_lossy(subcommand)))?;
        return Ok(());
    }
    if count != 3 {
        write_error(
            stream,
            &format!("wrong number of arguments for 'object|{}' command", String::from_utf8_lossy(subcommand).to_ascii_lowercase()),
        )?;
        return Ok(());
    }
    let Some(executor) = probe
    else {
        stream.write_all(b"-ERR unsupported command\r\n")?;
        return Ok(());
    };
    let key = request.arguments[2];
    // 集合键也参与 `OBJECT`：先解析逻辑键落在哪个类型命名空间，再按该命名空间计时。
    let (kind, target) = match resolve_kind(stream, executor, namespace, key)? {
        Resolved::Missing => return write_null(stream, protocol),
        Resolved::Kind(kind) => (kind, kind.namespace(namespace)),
        Resolved::Rejected => return Ok(()),
    };
    if subcommand.eq_ignore_ascii_case(b"ENCODING") {
        let command = database_query::cluster_kv::KeyValueCommand {
            namespace: target,
            key: key.to_vec(),
            action: database_query::cluster_kv::KeyValueAction::Get,
        };
        let value = match executor.execute(None, command) {
            Ok(KeyValueResult::Get(value)) => value,
            Ok(_) => {
                write_error(stream, "OBJECT ENCODING executor returned unexpected result")?;
                return Ok(());
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(());
            }
        };
        let Some(value) = value
        else {
            return write_null(stream, protocol);
        };
        return write_bulk(stream, object_encoding(kind, key, &value).as_bytes());
    }
    if subcommand.eq_ignore_ascii_case(b"IDLETIME") {
        // 纯内存计时：不写 `.yykv`、不升格式版本，重启后从零开始，与真机 RDB 的行为一致。
        // 无连接计时的调用方（绑定原语）看到的是一个刚建立的连接，空闲时间从 0 起算。
        let idle = tracker.map(|tracker| tracker.idle_seconds(&target, key)).unwrap_or(0);
        write!(stream, ":{idle}\r\n")
    }
    else if subcommand.eq_ignore_ascii_case(b"REFCOUNT") {
        // 这是由存储结构推出的结果，不是写死的常量：每条记录在 `Record` 里独占自己的
        // 值字节，既没有共享对象表也没有引用计数表，所以每个存在的键恰好只有一个引用。
        // 引入对象共享会改变表示与落盘口径，故不实现。
        stream.write_all(b":1\r\n")
    }
    else {
        // 网关没有淘汰策略配置面，恒为 LRU 口径：不做频率追踪，也不编造一个假计数。
        write_error(
            stream,
            "An LFU maxmemory policy is not selected, access frequency not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust.",
        )
    }
}

/// 真机 `OBJECT ENCODING` 的报告值。
///
/// 存储只保存逻辑值，编码由键名、内容与真机默认阈值推出。字符串按整数编码，
/// 其余按真机 8.10.3 的 `kvobj` 内嵌规则判定：键与值嵌在同一个 cache line
/// （`sizeof(robj)` 16 + 键长 + 3 + 值长 + 4 ≤ 64，即键长 + 值长 ≤ 41）时报
/// `embstr`，否则报 `raw`；过期元数据放在对象之前，不计入这个尺寸。
/// 集合类型按条目数、单条长度与整数成员判定。
fn object_encoding(kind: KeyKind, key: &[u8], value: &[u8]) -> &'static str {
    match kind {
        KeyKind::String => {
            if value.len() <= 20 && parse_integer(value).is_some() {
                "int"
            }
            else if key.len() + value.len() <= 41 {
                "embstr"
            }
            else {
                "raw"
            }
        }
        KeyKind::Hash => match decode_hash(value) {
            Some(fields) if fields.len() <= 128 && fields.iter().all(|(field, value)| field.len() <= 64 && value.len() <= 64) => "listpack",
            _ => "hashtable",
        },
        KeyKind::List => match decode_list(value) {
            // 真机的小列表按 `list-max-listpack-size -2`（8 KB）判定，而不是条目数。
            Some(elements) if elements.iter().map(|element| element.len() + 2).sum::<usize>() <= 8192 => "listpack",
            _ => "quicklist",
        },
        KeyKind::Set => match decode_set(value) {
            Some(members) if members.iter().all(|member| parse_integer(member).is_some()) && members.len() <= 512 => "intset",
            Some(members) if members.len() <= 128 && members.iter().all(|member| member.len() <= 64) => "listpack",
            _ => "hashtable",
        },
        KeyKind::ZSet => match decode_zset(value) {
            Some(entries) if entries.len() <= 128 && entries.iter().all(|(member, _)| member.len() <= 64) => "listpack",
            _ => "skiplist",
        },
        KeyKind::Stream => "stream",
    }
}

/// 字符串与键族的命令清单。
///
/// 这些命令共享同一条执行链：值读写、计数器、位域、过期、键搬移、枚举与库级操作。
/// 集合类型（hash / list / set / zset）不在其中，由各自的族入口承接。
pub(crate) fn is_string_command(command: &[u8]) -> bool {
    // 真机命令名大小写不敏感；先把命令名折叠成大写，再与清单逐字比较。
    let mut upper = [0u8; 16];
    if command.len() > upper.len() {
        return false;
    }
    upper[..command.len()].copy_from_slice(command);
    upper[..command.len()].make_ascii_uppercase();
    matches!(
        &upper[..command.len()],
        b"GET"
            | b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"GETDEL"
            | b"GETEX"
            | b"MGET"
            | b"MSET"
            | b"DEL"
            | b"EXISTS"
            | b"TYPE"
            | b"STRLEN"
            | b"APPEND"
            | b"GETRANGE"
            | b"SETRANGE"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"INCRBYFLOAT"
            | b"SETBIT"
            | b"GETBIT"
            | b"BITCOUNT"
            | b"BITPOS"
            | b"BITFIELD"
            | b"BITFIELD_RO"
            | b"TTL"
            | b"PTTL"
            | b"EXPIRE"
            | b"PEXPIRE"
            | b"EXPIREAT"
            | b"PEXPIREAT"
            | b"EXPIRETIME"
            | b"PEXPIRETIME"
            | b"PERSIST"
            | b"RENAME"
            | b"RENAMENX"
            | b"COPY"
            | b"KEYS"
            | b"SCAN"
            | b"RANDOMKEY"
            | b"DBSIZE"
            | b"FLUSHDB"
            | b"FLUSHALL"
            | b"OBJECT"
    )
}

/// 键空间族（`KEYS` / `SCAN` / `TYPE` / `OBJECT`）的命令清单。
///
/// 四个命令都只读键空间元信息：枚举键名、按游标分批返回、报告类型与对象编码。
/// 它们与字符串共享同一套类型探测与过期判定，单独成族只是为了给绑定层一个稳定入口。
pub(crate) fn is_keyspace_command(command: &[u8]) -> bool {
    // 与字符串族一致：命令名按真机口径大小写不敏感。
    let mut upper = [0u8; 16];
    if command.len() > upper.len() {
        return false;
    }
    upper[..command.len()].copy_from_slice(command);
    upper[..command.len()].make_ascii_uppercase();
    matches!(&upper[..command.len()], b"KEYS" | b"SCAN" | b"TYPE" | b"OBJECT")
}

/// 服务键空间族，返回 `false` 表示命令不属于该族。
pub fn handle_keyspace(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<bool> {
    let command = request.arguments[0];
    if !is_keyspace_command(command) {
        return Ok(false);
    }
    let count = request.arguments.len();
    if command.eq_ignore_ascii_case(b"TYPE") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'type' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Kind(kind) => stream.write_all(format!("+{}\r\n", kind.name()).as_bytes())?,
            Resolved::Missing => stream.write_all(b"+none\r\n")?,
            Resolved::Rejected => return Ok(true),
        }
    }
    else if command.eq_ignore_ascii_case(b"KEYS") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 2 {
            write_error(stream, "wrong number of arguments for 'keys' command")?;
            return Ok(true);
        }
        match collect_keys(executor, namespace) {
            Ok(keys) => {
                let matched: Vec<Vec<u8>> = keys.into_iter().filter(|key| glob_match(request.arguments[1], key)).collect();
                stream.write_all(format!("*{}\r\n", matched.len()).as_bytes())?;
                for key in &matched {
                    write_bulk(stream, key)?;
                }
            }
            Err(message) => write_error(stream, &message)?,
        }
    }
    else if command.eq_ignore_ascii_case(b"SCAN") {
        handle_scan(stream, request, executor, namespace)?;
    }
    else {
        handle_object(stream, request, probe, tracker, namespace, protocol)?;
    }
    Ok(true)
}

/// Serves the string and key family, returning `false` when the command belongs to a
/// different family so the caller keeps dispatching.
pub fn handle_string(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<bool> {
    let command = request.arguments[0];
    let count = request.arguments.len();
    if !is_string_command(command) {
        return Ok(false);
    }
    // 键空间族的四个命令曾经散落在这条长链里，抽成独立族后仍从这里分派，
    // 因此旧调用方（监听器与字符串绑定原语）行为不变。
    if is_keyspace_command(command) {
        return handle_keyspace(stream, request, executor, probe, tracker, namespace, protocol);
    }
    if command.eq_ignore_ascii_case(b"EXPIRE") || command.eq_ignore_ascii_case(b"PEXPIRE") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'expire' command\r\n")?;
            return Ok(true);
        }
        let flags = match parse_expire_flags(&request.arguments) {
            Ok(flags) => flags,
            Err(error) => {
                write_expire_flag_error(stream, error)?;
                return Ok(true);
            }
        };
        let key = request.arguments[1];
        let delta = match parse_integer(request.arguments[2]) {
            Some(v) => v,
            None => {
                stream.write_all(b"-ERR value is not an integer or out of range\r\n")?;
                return Ok(true);
            }
        };
        let target = match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Rejected => return Ok(true),
            Resolved::Missing => {
                stream.write_all(b":0\r\n")?;
                return Ok(true);
            }
            Resolved::Kind(kind) => kind.namespace(namespace),
        };
        let unit_ms: i64 = if command.eq_ignore_ascii_case(b"PEXPIRE") { 1 } else { 1000 };
        let Some(offset) = delta.checked_mul(unit_ms)
        else {
            stream.write_all(b"-ERR invalid expire time in 'expire' command\r\n")?;
            return Ok(true);
        };
        let now = now_unix_ms();
        let expires_at = now.saturating_add(offset);
        // 条件先于删除生效：`GT` 在相对过去的截止时间上会拒绝，键必须被保留。
        match expire_condition_allows(stream, executor, &target, key, flags, expires_at)? {
            None => return Ok(true),
            Some(false) => {
                stream.write_all(b":0\r\n")?;
                return Ok(true);
            }
            Some(true) => {}
        }
        // Non-positive or past deadlines delete the key, matching Redis.
        if expires_at <= now {
            let removed = match executor.execute(
                None,
                database_query::cluster_kv::KeyValueCommand {
                    namespace: target.clone(),
                    key: key.to_vec(),
                    action: database_query::cluster_kv::KeyValueAction::Delete,
                },
            ) {
                Ok(KeyValueResult::Delete { removed }) => removed,
                Ok(_) => {
                    write_error(stream, "EXPIRE executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            stream.write_all(if removed { b":1\r\n" } else { b":0\r\n" })?;
            return Ok(true);
        }
        let applied = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: target.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at },
            },
        ) {
            Ok(KeyValueResult::ExpiryChanged { applied }) => applied,
            Ok(_) => {
                write_error(stream, "EXPIRE executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        stream.write_all(if applied { b":1\r\n" } else { b":0\r\n" })?;
    }
    else if command.eq_ignore_ascii_case(b"EXPIREAT") || command.eq_ignore_ascii_case(b"PEXPIREAT") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'expireat' command\r\n")?;
            return Ok(true);
        }
        let flags = match parse_expire_flags(&request.arguments) {
            Ok(flags) => flags,
            Err(error) => {
                write_expire_flag_error(stream, error)?;
                return Ok(true);
            }
        };
        let key = request.arguments[1];
        let at = match parse_integer(request.arguments[2]) {
            Some(v) => v,
            None => {
                stream.write_all(b"-ERR value is not an integer or out of range\r\n")?;
                return Ok(true);
            }
        };
        let target = match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Rejected => return Ok(true),
            Resolved::Missing => {
                stream.write_all(b":0\r\n")?;
                return Ok(true);
            }
            Resolved::Kind(kind) => kind.namespace(namespace),
        };
        let unit_ms: i64 = if command.eq_ignore_ascii_case(b"PEXPIREAT") { 1 } else { 1000 };
        let Some(expires_at) = at.checked_mul(unit_ms)
        else {
            stream.write_all(b"-ERR invalid expire time in 'expireat' command\r\n")?;
            return Ok(true);
        };
        let now = now_unix_ms();
        // 条件先于删除生效，理由同 `EXPIRE`。
        match expire_condition_allows(stream, executor, &target, key, flags, expires_at)? {
            None => return Ok(true),
            Some(false) => {
                stream.write_all(b":0\r\n")?;
                return Ok(true);
            }
            Some(true) => {}
        }
        // Non-positive or past absolute deadlines delete the key, matching Redis.
        if expires_at <= now {
            let removed = match executor.execute(
                None,
                database_query::cluster_kv::KeyValueCommand {
                    namespace: target.clone(),
                    key: key.to_vec(),
                    action: database_query::cluster_kv::KeyValueAction::Delete,
                },
            ) {
                Ok(KeyValueResult::Delete { removed }) => removed,
                Ok(_) => {
                    write_error(stream, "EXPIREAT executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            stream.write_all(if removed { b":1\r\n" } else { b":0\r\n" })?;
            return Ok(true);
        }
        let applied = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: target.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at },
            },
        ) {
            Ok(KeyValueResult::ExpiryChanged { applied }) => applied,
            Ok(_) => {
                write_error(stream, "EXPIREAT executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        stream.write_all(if applied { b":1\r\n" } else { b":0\r\n" })?;
    }
    else if command.eq_ignore_ascii_case(b"MGET") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        match bind_mget(&request.arguments, namespace.0.as_str()) {
            Ok(commands) => {
                let commands: Vec<KeyValueCommand> = commands.into_iter().map(to_kv_command).collect();
                // Expired records are filtered by the KV layer itself, so the batch read
                // already reports them as absent.
                match executor.execute_read_batch(None, &commands) {
                    Ok(results) => {
                        if results.len() != commands.len() || results.iter().any(|result| !matches!(result, KeyValueResult::Get(_))) {
                            write_error(stream, "MGET executor returned an invalid read batch")?;
                            return Ok(true);
                        }
                        // 字符串缺失的键可能以 hash/list 形式存在，真机对这种情况整条命令回
                        // WRONGTYPE，而不是把该位置当 nil。
                        for (command, result) in commands.iter().zip(&results) {
                            if !matches!(result, KeyValueResult::Get(None)) {
                                continue;
                            }
                            match collections::find_kind(executor, namespace, &command.key) {
                                Ok(Some(KeyKind::String)) | Ok(None) => {}
                                Ok(Some(_)) => {
                                    collections::write_wrong_type(stream)?;
                                    return Ok(true);
                                }
                                Err(message) => {
                                    write_error(stream, &message)?;
                                    return Ok(true);
                                }
                            }
                        }
                        write!(stream, "*{}\r\n", results.len())?;
                        for result in results {
                            match result {
                                KeyValueResult::Get(Some(value)) => write_bulk(stream, &value)?,
                                KeyValueResult::Get(None) => write_null(stream, protocol)?,
                                _ => unreachable!(),
                            }
                        }
                    }
                    Err(error) => write_error(stream, &error.to_string())?,
                }
            }
            Err(BindError::WrongArity) => stream.write_all(b"-ERR wrong number of arguments\r\n")?,
            Err(BindError::EmptyCommand) => stream.write_all(b"-ERR empty command\r\n")?,
            Err(BindError::UnsupportedCommand) => stream.write_all(b"-ERR unsupported command\r\n")?,
            _ => stream.write_all(b"-ERR invalid MGET command\r\n")?,
        }
    }
    else if command.eq_ignore_ascii_case(b"EXISTS") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'exists' command\r\n")?;
            return Ok(true);
        }
        let mut present: i64 = 0;
        for key in &request.arguments[1..] {
            match resolve_kind(stream, executor, namespace, key)? {
                Resolved::Kind(_) => present += 1,
                Resolved::Missing => {}
                Resolved::Rejected => return Ok(true),
            }
        }
        write!(stream, ":{present}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"TTL") || command.eq_ignore_ascii_case(b"PTTL") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'ttl' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let target = match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Rejected => return Ok(true),
            Resolved::Missing => {
                stream.write_all(b":-2\r\n")?;
                return Ok(true);
            }
            Resolved::Kind(kind) => kind.namespace(namespace),
        };
        // `GetExpiry` reads through the KV layer, so a record whose deadline has passed
        // is already reported as absent.
        let (present, expires_at) = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: target,
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::GetExpiry,
            },
        ) {
            Ok(KeyValueResult::Expiry { present, expires_at }) => (present, expires_at),
            Ok(_) => {
                write_error(stream, "TTL executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        let value = if !present {
            -2
        }
        else {
            match expires_at {
                None => -1,
                Some(deadline) => {
                    let remaining_ms = deadline - now_unix_ms();
                    if remaining_ms <= 0 {
                        -2
                    }
                    else if command.eq_ignore_ascii_case(b"PTTL") {
                        remaining_ms
                    }
                    else {
                        // Ceiling division without the unstable `div_ceil`.
                        (remaining_ms + 999) / 1000
                    }
                }
            }
        };
        write!(stream, ":{value}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"STRLEN") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'strlen' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Kind(KeyKind::String) | Resolved::Missing => {}
            Resolved::Kind(_) => {
                collections::write_wrong_type(stream)?;
                return Ok(true);
            }
            Resolved::Rejected => return Ok(true),
        }
        let value = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: namespace.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::Get,
            },
        ) {
            Ok(KeyValueResult::Get(Some(value))) => value.len() as i64,
            Ok(KeyValueResult::Get(None)) => 0,
            Ok(_) => {
                write_error(stream, "STRLEN executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        write!(stream, ":{value}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"APPEND") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'append' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let append = request.arguments[2];
        match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Kind(KeyKind::String) | Resolved::Missing => {}
            Resolved::Kind(_) => {
                collections::write_wrong_type(stream)?;
                return Ok(true);
            }
            Resolved::Rejected => return Ok(true),
        }
        let current = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: namespace.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::Get,
            },
        ) {
            Ok(KeyValueResult::Get(Some(v))) => v,
            Ok(KeyValueResult::Get(None)) => Vec::new(),
            Ok(_) => {
                write_error(stream, "APPEND executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        let mut merged = current;
        merged.extend_from_slice(append);
        let new_len = merged.len() as i64;
        // APPEND keeps the TTL of the appended key. When the key is absent the record
        // simply has no expiry, so one expiry-preserving action covers both cases.
        match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: namespace.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::PutKeepingExpiry(merged),
            },
        ) {
            Ok(KeyValueResult::Put { .. }) => write!(stream, ":{new_len}\r\n")?,
            Ok(_) => {
                write_error(stream, "APPEND executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"GETRANGE") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 4 {
            stream.write_all(b"-ERR wrong number of arguments for 'getrange' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let (Some(start), Some(stop)) = (collections::parse_index(request.arguments[2]), collections::parse_index(request.arguments[3]))
        else {
            write_error(stream, "value is not an integer or out of range")?;
            return Ok(true);
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value,
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        match value.and_then(|bytes| normalized_range(start, stop, bytes.len()).map(|(begin, end)| bytes[begin..=end].to_vec())) {
            Some(slice) => write_bulk(stream, &slice)?,
            None => write_bulk(stream, b"")?,
        }
    }
    else if command.eq_ignore_ascii_case(b"SETRANGE") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 4 {
            stream.write_all(b"-ERR wrong number of arguments for 'setrange' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let replacement = request.arguments[3];
        // 真机区分两种失败：非规范整数报解析错误，负数报偏移越界。
        let offset = match std::str::from_utf8(request.arguments[2]).ok().and_then(|text| {
            let value = text.parse::<i64>().ok()?;
            (value.to_string() == text).then_some(value)
        }) {
            Some(value) if value >= 0 => value as u64,
            Some(_) => {
                write_error(stream, "offset is out of range")?;
                return Ok(true);
            }
            None => {
                write_error(stream, "value is not an integer or out of range")?;
                return Ok(true);
            }
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let current = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        // 空值只回当前长度，不创建键、也不改动已有内容。
        if replacement.is_empty() {
            write!(stream, ":{}\r\n", current.len())?;
            return Ok(true);
        }
        let end = offset.saturating_add(replacement.len() as u64);
        if end > MAX_STRING_BYTES {
            write_error(stream, "string exceeds maximum allowed size (proto-max-bulk-len)")?;
            return Ok(true);
        }
        let offset = offset as usize;
        let end = end as usize;
        let mut value = current;
        if value.len() < offset {
            value.resize(offset, 0);
        }
        if value.len() < end {
            value.resize(end, 0);
        }
        value[offset..end].copy_from_slice(replacement);
        // 回复是覆盖后的总长度，而不是写入区间的终点。
        let new_len = value.len();
        if let Err(message) = store_string(executor, namespace, key, value) {
            write_error(stream, &message)?;
            return Ok(true);
        }
        write!(stream, ":{new_len}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"INCRBYFLOAT") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'incrbyfloat' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let increment = match std::str::from_utf8(request.arguments[2]).ok().and_then(|text| text.parse::<f64>().ok()) {
            Some(increment) => increment,
            None => {
                write_error(stream, "value is not a valid float")?;
                return Ok(true);
            }
        };
        // 真机解析 `nan` 直接失败（字符串本身就是非法浮点），而 `+inf` / `-inf`
        // 能解析却在结果阶段被「非有限」规则拒绝。
        if increment.is_nan() {
            write_error(stream, "value is not a valid float")?;
            return Ok(true);
        }
        if increment.is_infinite() {
            write_error(stream, "increment would produce NaN or Infinity")?;
            return Ok(true);
        }
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let current = match load_string(executor, namespace, key) {
            Ok(None) => 0.0,
            Ok(Some(bytes)) => match std::str::from_utf8(&bytes).ok().and_then(|text| text.parse::<f64>().ok()) {
                // 存量值只拒绝 NaN 与非数字写法；inf 交给结果阶段报错。
                Some(value) if !value.is_nan() => value,
                _ => {
                    write_error(stream, "value is not a valid float")?;
                    return Ok(true);
                }
            },
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        let result = current + increment;
        if !result.is_finite() {
            write_error(stream, "increment would produce NaN or Infinity")?;
            return Ok(true);
        }
        let formatted = format_float(result);
        if let Err(message) = store_string(executor, namespace, key, formatted.clone().into_bytes()) {
            write_error(stream, &message)?;
            return Ok(true);
        }
        write_bulk(stream, formatted.as_bytes())?;
    }
    else if command.eq_ignore_ascii_case(b"SETBIT") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 4 {
            stream.write_all(b"-ERR wrong number of arguments for 'setbit' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let Some(offset) = parse_offset(request.arguments[2]).filter(|offset| *offset <= u32::MAX as u64)
        else {
            write_error(stream, "bit offset is not an integer or out of range")?;
            return Ok(true);
        };
        let bit = match request.arguments[3] {
            b"0" => false,
            b"1" => true,
            _ => {
                write_error(stream, "bit is not an integer or out of range")?;
                return Ok(true);
            }
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let mut value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        let byte = (offset / 8) as usize;
        let shift = 7 - (offset % 8) as u32;
        if value.len() <= byte {
            value.resize(byte + 1, 0);
        }
        let previous = (value[byte] >> shift) & 1;
        if bit {
            value[byte] |= 1 << shift;
        }
        else {
            value[byte] &= !(1 << shift);
        }
        if let Err(message) = store_string(executor, namespace, key, value) {
            write_error(stream, &message)?;
            return Ok(true);
        }
        write!(stream, ":{previous}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"GETBIT") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'getbit' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let Some(offset) = parse_offset(request.arguments[2]).filter(|offset| *offset <= u32::MAX as u64)
        else {
            write_error(stream, "bit offset is not an integer or out of range")?;
            return Ok(true);
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        let byte = (offset / 8) as usize;
        let bit = value.get(byte).map_or(0, |byte| (byte >> (7 - (offset % 8) as u32)) & 1);
        write!(stream, ":{bit}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"BITCOUNT") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if !matches!(count, 2 | 4 | 5) {
            stream.write_all(b"-ERR wrong number of arguments for 'bitcount' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let bit_unit = match count {
            5 if request.arguments[4].eq_ignore_ascii_case(b"BIT") => true,
            5 if request.arguments[4].eq_ignore_ascii_case(b"BYTE") => false,
            5 => {
                write_error(stream, "syntax error")?;
                return Ok(true);
            }
            _ => false,
        };
        let range = if count == 2 {
            None
        }
        else {
            let (Some(start), Some(stop)) = (collections::parse_index(request.arguments[2]), collections::parse_index(request.arguments[3]))
            else {
                write_error(stream, "value is not an integer or out of range")?;
                return Ok(true);
            };
            Some((start, stop))
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        let total = match range {
            None => value.iter().map(|byte| i64::from(byte.count_ones())).sum(),
            Some((start, stop)) => {
                let length = if bit_unit { value.len() * 8 } else { value.len() };
                match normalized_range(start, stop, length) {
                    None => 0,
                    Some((begin, end)) => {
                        if bit_unit {
                            count_set_bits(&value, begin, end)
                        }
                        else {
                            value[begin..=end].iter().map(|byte| i64::from(byte.count_ones())).sum()
                        }
                    }
                }
            }
        };
        write!(stream, ":{total}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"BITPOS") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if !(3..=6).contains(&count) {
            stream.write_all(b"-ERR wrong number of arguments for 'bitpos' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let target: u8 = match request.arguments[2] {
            b"0" => 0,
            b"1" => 1,
            _ => {
                write_error(stream, "The bit argument must be 1 or 0.")?;
                return Ok(true);
            }
        };
        let bit_unit = match count {
            6 if request.arguments[5].eq_ignore_ascii_case(b"BIT") => true,
            6 if request.arguments[5].eq_ignore_ascii_case(b"BYTE") => false,
            6 => {
                write_error(stream, "syntax error")?;
                return Ok(true);
            }
            _ => false,
        };
        let (start, stop, end_given) = match count {
            3 => (None, None, false),
            4 => (Some(request.arguments[3]), None, false),
            5 => (Some(request.arguments[3]), Some(request.arguments[4]), true),
            _ => (Some(request.arguments[3]), Some(request.arguments[4]), true),
        };
        let parsed_start = match start {
            None => None,
            Some(raw) => match collections::parse_index(raw) {
                Some(index) => Some(index),
                None => {
                    write_error(stream, "value is not an integer or out of range")?;
                    return Ok(true);
                }
            },
        };
        let parsed_stop = match stop {
            None => None,
            Some(raw) => match collections::parse_index(raw) {
                Some(index) => Some(index),
                None => {
                    write_error(stream, "value is not an integer or out of range")?;
                    return Ok(true);
                }
            },
        };
        if !ensure_string(stream, executor, namespace, key)? {
            return Ok(true);
        }
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value,
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        // 键不存在时真机把它视为无限长的 0 串，忽略范围：找 0 回 0，找 1 回 -1。
        let Some(value) = value
        else {
            let answer = if target == 0 { 0 } else { -1 };
            write!(stream, ":{answer}\r\n")?;
            return Ok(true);
        };
        // 真机在归一化之前先比原始负索引：两个负值且起点大于终点即空区间。
        if let (Some(start), Some(stop)) = (parsed_start, parsed_stop) {
            if start < 0 && stop < 0 && start > stop {
                write!(stream, ":-1\r\n")?;
                return Ok(true);
            }
        }
        let total_bits = (value.len() * 8) as i64;
        let units = if bit_unit { total_bits } else { value.len() as i64 };
        let begin_unit = parsed_start.map_or(0, |start| normalize_unit(start, units));
        let end_unit = parsed_stop.map_or(units - 1, |stop| normalize_unit(stop, units));
        if begin_unit > end_unit {
            write!(stream, ":-1\r\n")?;
            return Ok(true);
        }
        let (begin, end) = if bit_unit { (begin_unit, end_unit) } else { (begin_unit * 8, end_unit * 8 + 7) };
        match find_bit(&value, begin as usize, end as usize, target) {
            Some(bit) => write!(stream, ":{bit}\r\n")?,
            None => {
                // 没显式给终点且找 0 时，末尾之后的第一个位就是答案。
                let answer = if target == 0 && !end_given { total_bits } else { -1 };
                write!(stream, ":{answer}\r\n")?;
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"SET") {
        handle_set(stream, request, executor, namespace, protocol)?;
    }
    else if command.eq_ignore_ascii_case(b"PERSIST") || command.eq_ignore_ascii_case(b"SETEX") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if command.eq_ignore_ascii_case(b"PERSIST") {
            if count != 2 {
                stream.write_all(b"-ERR wrong number of arguments for 'persist' command\r\n")?;
                return Ok(true);
            }
            let key = request.arguments[1];
            let target = match resolve_kind(stream, executor, namespace, key)? {
                Resolved::Rejected => return Ok(true),
                Resolved::Missing => {
                    stream.write_all(b":0\r\n")?;
                    return Ok(true);
                }
                Resolved::Kind(kind) => kind.namespace(namespace),
            };
            let applied = match executor.execute(
                None,
                database_query::cluster_kv::KeyValueCommand {
                    namespace: target,
                    key: key.to_vec(),
                    action: database_query::cluster_kv::KeyValueAction::ClearExpiry,
                },
            ) {
                Ok(KeyValueResult::ExpiryChanged { applied }) => applied,
                Ok(_) => {
                    write_error(stream, "PERSIST executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            stream.write_all(if applied { b":1\r\n" } else { b":0\r\n" })?;
        }
        else {
            if count != 4 {
                stream.write_all(b"-ERR wrong number of arguments for 'setex' command\r\n")?;
                return Ok(true);
            }
            let Some(seconds) = parse_integer(request.arguments[2])
            else {
                write_error(stream, "value is not an integer or out of range")?;
                return Ok(true);
            };
            if seconds <= 0 {
                stream.write_all(b"-ERR invalid expire time in 'setex' command\r\n")?;
                return Ok(true);
            }
            let expires_at = now_unix_ms() + seconds.saturating_mul(1000);
            let result = executor.execute(
                None,
                database_query::cluster_kv::KeyValueCommand {
                    namespace: namespace.clone(),
                    key: request.arguments[1].to_vec(),
                    action: database_query::cluster_kv::KeyValueAction::PutExpiring { value: request.arguments[3].to_vec(), expires_at },
                },
            );
            match result {
                Ok(KeyValueResult::Put { .. }) => {
                    // SETEX 与 SET 一样无条件替换键，写完后清掉其它类型表示，保持一键一型。
                    if let Err(message) = clear_other_kinds(executor, namespace, request.arguments[1]) {
                        write_error(stream, &message)?;
                        return Ok(true);
                    }
                    stream.write_all(b"+OK\r\n")?;
                }
                Ok(_) => {
                    write_error(stream, "SETEX executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"GETEX") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'getex' command\r\n")?;
            return Ok(true);
        }
        /// One `GETEX` expiry option, kept unparsed until the key lookup decides whether
        /// its numeric argument is even relevant.
        #[derive(Clone, Copy)]
        enum ExpirySpec<'a> {
            Persist,
            Seconds(&'a [u8]),
            Milliseconds(&'a [u8]),
            SecondsAt(&'a [u8]),
            MillisecondsAt(&'a [u8]),
        }
        // Option structure (mutual exclusion and argument presence) is checked before the
        // lookup because Redis reports syntax errors even for a missing key. The numeric
        // value itself is checked after the lookup, so `GETEX missing EX not-a-number`
        // returns nil instead of an expiry error, as upstream does.
        let mut spec: Option<ExpirySpec<'_>> = None;
        let mut spec_kind: Option<&[u8]> = None;
        let mut index = 2;
        while index < count {
            let option = request.arguments[index];
            if spec_kind.is_some_and(|kind| !kind.eq_ignore_ascii_case(option)) {
                stream.write_all(b"-ERR syntax error\r\n")?;
                return Ok(true);
            }
            if option.eq_ignore_ascii_case(b"PERSIST") {
                spec = Some(ExpirySpec::Persist);
                spec_kind = Some(option);
                index += 1;
            }
            else if option.eq_ignore_ascii_case(b"EX")
                || option.eq_ignore_ascii_case(b"PX")
                || option.eq_ignore_ascii_case(b"EXAT")
                || option.eq_ignore_ascii_case(b"PXAT")
            {
                let Some(raw) = request.arguments.get(index + 1)
                else {
                    stream.write_all(b"-ERR syntax error\r\n")?;
                    return Ok(true);
                };
                spec = Some(if option.eq_ignore_ascii_case(b"EX") {
                    ExpirySpec::Seconds(raw)
                }
                else if option.eq_ignore_ascii_case(b"PX") {
                    ExpirySpec::Milliseconds(raw)
                }
                else if option.eq_ignore_ascii_case(b"EXAT") {
                    ExpirySpec::SecondsAt(raw)
                }
                else {
                    ExpirySpec::MillisecondsAt(raw)
                });
                spec_kind = Some(option);
                index += 2;
            }
            else {
                stream.write_all(b"-ERR syntax error\r\n")?;
                return Ok(true);
            }
        }
        let key = request.arguments[1];
        // GETEX is a string command: a hash/list key is WRONGTYPE rather than nil.
        match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Kind(KeyKind::String) | Resolved::Missing => {}
            Resolved::Kind(_) => {
                collections::write_wrong_type(stream)?;
                return Ok(true);
            }
            Resolved::Rejected => return Ok(true),
        }
        let value = match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: namespace.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::Get,
            },
        ) {
            Ok(KeyValueResult::Get(value)) => value,
            Ok(_) => {
                write_error(stream, "GETEX executor returned unexpected result")?;
                return Ok(true);
            }
            Err(error) => {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        };
        // An absent (or expired) key returns nil and nothing is touched, so the expiry
        // argument is deliberately left unparsed in that branch.
        let Some(value) = value
        else {
            write_null(stream, protocol)?;
            return Ok(true);
        };
        if let Some(spec) = spec {
            let action = match spec {
                ExpirySpec::Persist => database_query::cluster_kv::KeyValueAction::ClearExpiry,
                ExpirySpec::Seconds(raw) | ExpirySpec::Milliseconds(raw) | ExpirySpec::SecondsAt(raw) | ExpirySpec::MillisecondsAt(raw) => {
                    let Some(value) = parse_integer(raw)
                    else {
                        stream.write_all(b"-ERR value is not an integer or out of range\r\n")?;
                        return Ok(true);
                    };
                    if value <= 0 {
                        stream.write_all(b"-ERR invalid expire time in 'getex' command\r\n")?;
                        return Ok(true);
                    }
                    let expires_at = match spec {
                        ExpirySpec::Seconds(_) => value.checked_mul(1000).and_then(|delta_ms| now_unix_ms().checked_add(delta_ms)),
                        ExpirySpec::Milliseconds(_) => now_unix_ms().checked_add(value),
                        ExpirySpec::SecondsAt(_) => value.checked_mul(1000),
                        ExpirySpec::MillisecondsAt(_) => Some(value),
                        ExpirySpec::Persist => None,
                    };
                    let Some(expires_at) = expires_at
                    else {
                        stream.write_all(b"-ERR invalid expire time in 'getex' command\r\n")?;
                        return Ok(true);
                    };
                    database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at }
                }
            };
            if let Err(error) =
                executor.execute(None, database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action })
            {
                write_error(stream, &error.to_string())?;
                return Ok(true);
            }
        }
        write_bulk(stream, &value)?;
    }
    else if command.eq_ignore_ascii_case(b"RENAME") || command.eq_ignore_ascii_case(b"RENAMENX") {
        // Session / token rotation: the record moves together with its remaining deadline in
        // one shard-local durable commit, so no read-write-delete window can lose it and the
        // TTL is never reset to "fresh" by the move.
        //
        // 类型编码在命名空间上，因此先解析源键的实际类型，再在**该类型命名空间**内搬移，
        // 源键的值类型与剩余过期时间都原样跟着走。
        let rename_nx = command.eq_ignore_ascii_case(b"RENAMENX");
        let name = if rename_nx { "renamenx" } else { "rename" };
        if request.arguments.len() != 3 {
            write!(stream, "-ERR wrong number of arguments for '{name}' command\r\n")?;
            return Ok(true);
        }
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        let source = request.arguments[1];
        let destination = request.arguments[2];
        let kind = match collections::find_kind(executor, namespace, source) {
            Ok(Some(kind)) => kind,
            Ok(None) => {
                write_error(stream, "no such key")?;
                return Ok(true);
            }
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        if source == destination {
            if rename_nx {
                stream.write_all(b":0\r\n")?;
            }
            else {
                stream.write_all(b"+OK\r\n")?;
            }
            return Ok(true);
        }
        let destination_exists = match collections::find_kind(executor, namespace, destination) {
            Ok(exists) => exists.is_some(),
            Err(message) => {
                write_error(stream, &message)?;
                return Ok(true);
            }
        };
        if rename_nx && destination_exists {
            stream.write_all(b":0\r\n")?;
            return Ok(true);
        }
        if destination_exists {
            // 覆盖目标时先清掉它可能存在的其它类型表示，避免一次逻辑键出现两份值。
            for other in KeyKind::ALL {
                let command =
                    KeyValueCommand { namespace: other.namespace(namespace), key: destination.to_vec(), action: KeyValueAction::Delete };
                if let Err(error) = executor.execute(None, command) {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            }
        }
        let action = KeyValueAction::Rename { destination: destination.to_vec(), if_absent: false };
        let command = KeyValueCommand { namespace: kind.namespace(namespace), key: source.to_vec(), action };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Rename { source_present: true, .. }) => {
                if rename_nx {
                    stream.write_all(b":1\r\n")?;
                }
                else {
                    stream.write_all(b"+OK\r\n")?;
                }
            }
            Ok(KeyValueResult::Rename { source_present: false, .. }) => write_error(stream, "no such key")?,
            Ok(_) => write_error(stream, "RENAME executor returned unexpected result")?,
            Err(error) => write_error(stream, &error.to_string())?,
        }
    }
    else if command.eq_ignore_ascii_case(b"DEL") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'del' command\r\n")?;
            return Ok(true);
        };
        // 一个逻辑键可能以字符串、hash 或 list 中的任意一种形式存在，删除要逐类型清理；
        // 只要其中一种表示被删掉，这次 `DEL` 就对它计 1，与真机的按键计数一致。
        let mut removed: i64 = 0;
        for key in &request.arguments[1..] {
            let mut deleted = false;
            for kind in KeyKind::ALL {
                let command = database_query::cluster_kv::KeyValueCommand {
                    namespace: kind.namespace(namespace),
                    key: key.to_vec(),
                    action: database_query::cluster_kv::KeyValueAction::Delete,
                };
                match executor.execute(None, command) {
                    Ok(KeyValueResult::Delete { removed: true }) => deleted = true,
                    Ok(KeyValueResult::Delete { removed: false }) => {}
                    Ok(_) => {
                        write_error(stream, "DEL executor returned unexpected result")?;
                        return Ok(true);
                    }
                    Err(error) => {
                        write_error(stream, &error.to_string())?;
                        return Ok(true);
                    }
                }
            }
            if deleted {
                removed += 1;
            }
        }
        write!(stream, ":{removed}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"RANDOMKEY") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 1 {
            write_error(stream, "wrong number of arguments for 'randomkey' command")?;
            return Ok(true);
        }
        match collect_keys(executor, namespace) {
            Ok(keys) if keys.is_empty() => write_null(stream, protocol)?,
            Ok(keys) => {
                let index = random_index(keys.len());
                write_bulk(stream, &keys[index])?;
            }
            Err(message) => write_error(stream, &message)?,
        }
    }
    else if command.eq_ignore_ascii_case(b"DBSIZE") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 1 {
            write_error(stream, "wrong number of arguments for 'dbsize' command")?;
            return Ok(true);
        }
        match collect_keys(executor, namespace) {
            Ok(keys) => write!(stream, ":{}\r\n", keys.len())?,
            Err(message) => write_error(stream, &message)?,
        }
    }
    else if command.eq_ignore_ascii_case(b"FLUSHALL") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count > 2 {
            write_error(stream, "wrong number of arguments for 'flushall' command")?;
            return Ok(true);
        }
        if count == 2 && !request.arguments[1].eq_ignore_ascii_case(b"ASYNC") && !request.arguments[1].eq_ignore_ascii_case(b"SYNC") {
            write_error(stream, "syntax error")?;
            return Ok(true);
        }
        let bases = (0..16u32).map(|database| database_namespace(namespace, database));
        match flush_databases(executor, bases) {
            Some(message) => write_error(stream, &message)?,
            None => stream.write_all(b"+OK\r\n")?,
        }
    }
    else if command.eq_ignore_ascii_case(b"FLUSHDB") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count > 2 {
            write_error(stream, "wrong number of arguments for 'flushdb' command")?;
            return Ok(true);
        }
        if count == 2 && !request.arguments[1].eq_ignore_ascii_case(b"ASYNC") && !request.arguments[1].eq_ignore_ascii_case(b"SYNC") {
            write_error(stream, "syntax error")?;
            return Ok(true);
        }
        match flush_databases(executor, [namespace.clone()]) {
            Some(message) => write_error(stream, &message)?,
            None => stream.write_all(b"+OK\r\n")?,
        }
    }
    else if command.eq_ignore_ascii_case(b"COPY") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        handle_copy(stream, request, executor, namespace)?;
    }
    else if command.eq_ignore_ascii_case(b"EXPIRETIME") || command.eq_ignore_ascii_case(b"PEXPIRETIME") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 2 {
            let name = if command.eq_ignore_ascii_case(b"PEXPIRETIME") { "pexpiretime" } else { "expiretime" };
            write_error(stream, &format!("wrong number of arguments for '{name}' command"))?;
            return Ok(true);
        }
        let key = request.arguments[1];
        let target = match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Rejected => return Ok(true),
            Resolved::Missing => {
                stream.write_all(b":-2\r\n")?;
                return Ok(true);
            }
            Resolved::Kind(kind) => kind.namespace(namespace),
        };
        let (present, expires_at) =
            match executor.execute(None, KeyValueCommand { namespace: target, key: key.to_vec(), action: KeyValueAction::GetExpiry }) {
                Ok(KeyValueResult::Expiry { present, expires_at }) => (present, expires_at),
                Ok(_) => {
                    write_error(stream, "EXPIRETIME executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
        let value = if !present {
            -2
        }
        else {
            match expires_at {
                None => -1,
                Some(deadline) => {
                    if deadline <= now_unix_ms() {
                        -2
                    }
                    else if command.eq_ignore_ascii_case(b"PEXPIRETIME") {
                        deadline
                    }
                    else {
                        // 与真机一致：`EXPIRETIME` 按毫秒余数四舍五入到秒，余数 >= 500 进位
                        (deadline + 500) / 1000
                    }
                }
            }
        };
        write!(stream, ":{value}\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"SETNX") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 3 {
            stream.write_all(b"-ERR wrong number of arguments for 'setnx' command\r\n")?;
            return Ok(true);
        }
        let key = request.arguments[1];
        match resolve_kind(stream, executor, namespace, key)? {
            Resolved::Rejected => {}
            // 任意类型的旧键都算「已存在」，真机不会因为类型不同而报错。
            Resolved::Kind(_) => stream.write_all(b":0\r\n")?,
            Resolved::Missing => {
                let action = database_query::cluster_kv::KeyValueAction::PutIfAbsent(request.arguments[2].to_vec());
                let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action };
                match executor.execute(None, command) {
                    Ok(KeyValueResult::PutIfAbsent { revision }) => stream.write_all(if revision.is_some() { b":1\r\n" } else { b":0\r\n" })?,
                    Ok(_) => write_error(stream, "SETNX executor returned unexpected result")?,
                    Err(error) => write_error(stream, &error.to_string())?,
                }
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"PSETEX") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count != 4 {
            stream.write_all(b"-ERR wrong number of arguments for 'psetex' command\r\n")?;
            return Ok(true);
        }
        let Some(milliseconds) = parse_integer(request.arguments[2])
        else {
            write_error(stream, "value is not an integer or out of range")?;
            return Ok(true);
        };
        if milliseconds <= 0 {
            write_error(stream, "invalid expire time in 'psetex' command")?;
            return Ok(true);
        }
        let expires_at = now_unix_ms().saturating_add(milliseconds);
        let action = database_query::cluster_kv::KeyValueAction::PutExpiring { value: request.arguments[3].to_vec(), expires_at };
        let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: request.arguments[1].to_vec(), action };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Put { .. }) => {
                if let Err(message) = clear_other_kinds(executor, namespace, request.arguments[1]) {
                    write_error(stream, &message)?;
                    return Ok(true);
                }
                stream.write_all(b"+OK\r\n")?;
            }
            Ok(_) => write_error(stream, "PSETEX executor returned unexpected result")?,
            Err(error) => write_error(stream, &error.to_string())?,
        }
    }
    else if command.eq_ignore_ascii_case(b"MSET") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        if count < 3 || count % 2 == 0 {
            stream.write_all(b"-ERR wrong number of arguments for 'mset' command\r\n")?;
            return Ok(true);
        }
        for pair in request.arguments[1..].chunks(2) {
            let key = pair[0];
            let action = database_query::cluster_kv::KeyValueAction::Put(pair[1].to_vec());
            let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action };
            match executor.execute(None, command) {
                Ok(KeyValueResult::Put { .. }) => {
                    if let Err(message) = clear_other_kinds(executor, namespace, key) {
                        write_error(stream, &message)?;
                        return Ok(true);
                    }
                }
                Ok(_) => {
                    write_error(stream, "MSET executor returned unexpected result")?;
                    return Ok(true);
                }
                Err(error) => {
                    write_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            }
        }
        stream.write_all(b"+OK\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"BITFIELD") || command.eq_ignore_ascii_case(b"BITFIELD_RO") {
        let Some(executor) = executor
        else {
            stream.write_all(b"-ERR unsupported command\r\n")?;
            return Ok(true);
        };
        collections::handle_bitfield(stream, executor, namespace, &request.arguments, command.eq_ignore_ascii_case(b"BITFIELD_RO"), protocol)?;
        return Ok(true);
    }
    else if let Some(executor) = executor {
        // --- Generic bind()-backed commands (GET, INCR, DECR, GETDEL, GETSET).
        // Expiry semantics are enforced by the KV layer: expired records read as absent
        // and each action carries its own expiry behaviour.
        match bind(&request.arguments, namespace.0.as_str()) {
            Ok(command) => {
                // 这些命令只对字符串有效：hash / list 键在真机上回 WRONGTYPE 而不是 nil。
                // 绑定成功后再探测，保证参数错误优先于类型错误。
                if matches!(command.action, BoundAction::Get | BoundAction::GetDelete | BoundAction::GetSet(_) | BoundAction::IncrementBy(_)) {
                    match resolve_kind(stream, executor, namespace, &command.key)? {
                        Resolved::Kind(KeyKind::String) | Resolved::Missing => {}
                        Resolved::Kind(_) => {
                            collections::write_wrong_type(stream)?;
                            return Ok(true);
                        }
                        Resolved::Rejected => return Ok(true),
                    }
                }
                match executor.execute(None, to_kv_command(command)) {
                    Ok(result) => {
                        match result {
                            KeyValueResult::Get(Some(value)) => write_bulk(stream, &value)?,
                            KeyValueResult::Get(None) => write_null(stream, protocol)?,
                            KeyValueResult::Exists(exists) => stream.write_all(if exists { b":1\r\n" } else { b":0\r\n" })?,
                            KeyValueResult::GetDelete(Some(value)) => write_bulk(stream, &value)?,
                            KeyValueResult::GetDelete(None) => write_null(stream, protocol)?,
                            KeyValueResult::GetSet(Some(value)) => write_bulk(stream, &value)?,
                            KeyValueResult::GetSet(None) => write_null(stream, protocol)?,
                            KeyValueResult::Put { .. } => stream.write_all(b"+OK\r\n")?,
                            KeyValueResult::PutIfAbsent { revision: Some(_) } => stream.write_all(b"+OK\r\n")?,
                            KeyValueResult::PutIfAbsent { revision: None } => write_null(stream, protocol)?,
                            KeyValueResult::PutIfPresent { revision: Some(_) } => stream.write_all(b"+OK\r\n")?,
                            KeyValueResult::PutIfPresent { revision: None } => write_null(stream, protocol)?,
                            KeyValueResult::Increment { value } => write!(stream, ":{value}\r\n")?,
                            KeyValueResult::Delete { removed } => stream.write_all(if removed { b":1\r\n" } else { b":0\r\n" })?,
                            // Expiry, revision and rename results only surface through their
                            // dedicated paths, never through the generic bind() commands.
                            KeyValueResult::Expiry { .. }
                            | KeyValueResult::ExpiryChanged { .. }
                            | KeyValueResult::Revision(_)
                            | KeyValueResult::Rename { .. } => {
                                write_error(stream, "unexpected result from bind command")?;
                                return Ok(true);
                            }
                        }
                    }
                    Err(error) => write_error(stream, &error.to_string())?,
                }
            }
            Err(BindError::WrongArity) => stream.write_all(b"-ERR wrong number of arguments\r\n")?,
            Err(BindError::Syntax) => stream.write_all(b"-ERR syntax error\r\n")?,
            Err(BindError::UnsupportedOptions) => stream.write_all(b"-ERR syntax error\r\n")?,
            Err(BindError::InvalidInteger) => stream.write_all(b"-ERR value is not an integer or out of range\r\n")?,
            Err(BindError::InvalidExpire) => stream.write_all(b"-ERR invalid expire time in 'set' command\r\n")?,
            Err(BindError::EmptyCommand) => stream.write_all(b"-ERR empty command\r\n")?,
            Err(BindError::UnsupportedCommand) => write_unknown_command(stream, &request.arguments)?,
        }
    }
    Ok(true)
}

/// 用一帧参数执行字符串与键族命令，未知命令按真机文案回复。
///
/// 这是给持有整帧的嵌入宿主用的族入口：监听器按帧分派时用 [`handle_string`] 的
/// `false` 返回值继续找下一个族，宿主只有一条命令要执行时用这里。命令语义、
/// 参数校验、过期判定与回复分帧都在本模块，调用方只提供逻辑库命名空间与协议版本。
pub fn execute(
    stream: &mut dyn Write,
    arguments: &[&[u8]],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<()> {
    let request = Request { arguments: arguments.to_vec(), consumed: 0 };
    if handle_string(stream, &request, Some(executor), Some(executor), None, namespace, protocol)? {
        return Ok(());
    }
    write_unknown_command(stream, arguments)
}

/// 用一帧参数执行键空间族（`KEYS` / `SCAN` / `TYPE` / `OBJECT`），未知命令按真机文案回复。
pub fn execute_keyspace(
    stream: &mut dyn Write,
    arguments: &[&[u8]],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
) -> io::Result<()> {
    let request = Request { arguments: arguments.to_vec(), consumed: 0 };
    if handle_keyspace(stream, &request, Some(executor), Some(executor), None, namespace, protocol)? {
        return Ok(());
    }
    write_unknown_command(stream, arguments)
}

/// 一个逻辑库里可见的键数与其中带过期时间的键数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyspaceCount {
    /// 逻辑库序号。
    pub database: u32,
    /// 该库里可见的键数（跨全部值类型）。
    pub keys: usize,
    /// 其中带过期时间的键数。
    pub expires: usize,
}

/// 统计 16 个逻辑库的键空间占用，只返回有键的库。
///
/// `INFO keyspace` 的取值口径：键按值类型命名空间分别枚举后累加，过期数逐键读取
/// 截止时间；读取失败的库跳过而不是编造数字。段落排版留给服务端面。
pub fn keyspace_stats(executor: &dyn KeyValueExecutor) -> Vec<KeyspaceCount> {
    let mut databases = Vec::new();
    for database in 0..16u32 {
        let base = Namespace(format!("redis:{database}"));
        let mut keys = 0_usize;
        let mut expires = 0_usize;
        for kind in KeyKind::ALL {
            let target = kind.namespace(&base);
            let Ok(records) = executor.execute_scan(None, &target, &[], None, None, 0)
            else {
                continue;
            };
            keys += records.len();
            for (key, _) in records {
                let command = KeyValueCommand { namespace: target.clone(), key, action: KeyValueAction::GetExpiry };
                if let Ok(KeyValueResult::Expiry { present: true, expires_at: Some(_) }) = executor.execute(None, command) {
                    expires += 1;
                }
            }
        }
        if keys > 0 {
            databases.push(KeyspaceCount { database, keys, expires });
        }
    }
    databases
}
