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
    sync::Mutex,
    time::Instant,
};

use database_protocol_redis::{
    bind::{BindError, BoundAction, BoundCommand, SetCondition, SetExpiry, bind, bind_mget, bind_set},
    codec::{CommandError, Reply},
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
    reply::{glob_match, unknown_command_error},
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

/// `SET` 选项列表被拒时的语义错误，由 protocol 编码为 `-ERR ...`。
fn set_bind_error(error: BindError) -> CommandError {
    match error {
        BindError::WrongArity => CommandError::Failed("wrong number of arguments for 'set' command".to_owned()),
        BindError::Syntax => CommandError::Failed("syntax error".to_owned()),
        BindError::InvalidInteger => CommandError::Failed("value is not an integer or out of range".to_owned()),
        BindError::InvalidExpire => CommandError::Failed("invalid expire time in 'set' command".to_owned()),
        BindError::UnsupportedOptions => CommandError::Failed("syntax error".to_owned()),
        BindError::EmptyCommand => CommandError::Failed("empty command".to_owned()),
        BindError::UnsupportedCommand => CommandError::Failed("unsupported command".to_owned()),
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
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    let parsed = match bind_set(&request.arguments) {
        Ok(parsed) => parsed,
        Err(error) => return Err(set_bind_error(error)),
    };
    // SET 会无条件替换键，因此条件判断必须看到集合类型：`NX` 在 hash 上要回 nil，
    // `XX` 在 hash 上要照常替换。字符串命名空间为空时 `PutIfPresent` 会误报未命中，
    // 所以非字符串上的 XX 改走无条件写入。
    let existing = resolve_kind(executor, namespace, &parsed.key)?;
    // `SET ... GET` 先取旧值：真机在读不到字符串（键为集合）时直接 WRONGTYPE，
    // 不执行写入，也不再看 NX / XX 条件。
    let old_value = if parsed.get {
        match existing {
            Some(KeyKind::String) => match load_string(executor, namespace, &parsed.key) {
                Ok(value) => value,
                Err(message) => {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            },
            Some(_) => return Err(CommandError::WrongType),
            None => None,
        }
    }
    else {
        None
    };
    let condition = match parsed.condition {
        SetCondition::IfAbsent if existing.is_some() => return Ok(set_reply(parsed.get, &old_value, false)),
        SetCondition::IfPresent if existing.is_none() => return Ok(set_reply(parsed.get, &old_value, false)),
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
            None => return Err(set_bind_error(BindError::InvalidExpire)),
        },
        // NX on an absent key has no existing deadline to keep, so plain PutIfAbsent is exact.
        (SetCondition::IfAbsent, Some(expiry)) => match set_deadline(expiry) {
            Some(expires_at) => database_query::cluster_kv::KeyValueAction::PutIfAbsentExpiring { value: parsed.value, expires_at },
            None => return Err(set_bind_error(BindError::InvalidExpire)),
        },
        (SetCondition::IfAbsent, None) => database_query::cluster_kv::KeyValueAction::PutIfAbsent(parsed.value),
        (SetCondition::IfPresent, Some(expiry)) => match set_deadline(expiry) {
            Some(expires_at) => database_query::cluster_kv::KeyValueAction::PutIfPresentExpiring { value: parsed.value, expires_at },
            None => return Err(set_bind_error(BindError::InvalidExpire)),
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
            return Err(CommandError::Failed("SET executor returned unexpected result".to_owned()));
        }
        Err(error) => {
            return Err(CommandError::Failed(error.to_string().to_owned()));
        }
    };
    if !stored {
        // The condition failed: no write happened, so any existing deadline survives.
        return Ok(set_reply(parsed.get, &old_value, false));
    }
    if let Some(expires_at) = inherited_expiry {
        let action = database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at };
        let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: parsed.key.clone(), action };
        if let Err(error) = executor.execute(None, command) {
            return Err(CommandError::Failed(error.to_string().to_owned()));
        }
    }
    // 覆盖写成功后清掉旧的集合表示，保证一个逻辑键只有一种类型。
    if let Err(message) = clear_other_kinds(executor, namespace, &parsed.key) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    Ok(set_reply(parsed.get, &old_value, true))
}

/// `SET` 的回复：带 `GET` 时回旧值（缺失为 nil），否则成功回 `+OK`、条件不满足回 nil。
fn set_reply(get: bool, old_value: &Option<Vec<u8>>, success: bool) -> Reply {
    if get {
        return match old_value {
            Some(value) => Reply::bulk(value.clone()),
            None => Reply::nil(),
        };
    }
    if success { Reply::ok() } else { Reply::nil() }
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

/// 探测逻辑键当前类型；`Ok(None)` 表示键不存在，探测失败转成可回复的语义错误。
fn resolve_kind(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<Option<KeyKind>, CommandError> {
    collections::find_kind(executor, namespace, key).map_err(|message| CommandError::Failed(message.to_owned()))
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

/// 断言逻辑键当前是字符串或不存在；其它类型回 `WRONGTYPE`。
fn ensure_string(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<(), CommandError> {
    match resolve_kind(executor, namespace, key)? {
        None | Some(KeyKind::String) => Ok(()),
        Some(_) => Err(CommandError::WrongType),
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

/// 条件选项错误的语义消息，与真机逐字一致。
fn expire_flag_error(error: ExpireFlagError) -> CommandError {
    match error {
        ExpireFlagError::Conflict => CommandError::Failed("NX and XX, GT or LT options at the same time are not compatible".to_owned()),
        ExpireFlagError::GtLtConflict => CommandError::Failed("GT and LT options at the same time are not compatible".to_owned()),
        ExpireFlagError::Unsupported(option) => {
            let text = String::from_utf8_lossy(&option);
            CommandError::Failed(format!("Unsupported option {text}"))
        }
    }
}

/// 判定条件选项是否允许把截止时间设为 `expires_at`。
///
/// 真机的条件是相对「当前截止时间」比较：`NX` 要求当前无过期，`XX` 要求当前有过期，
/// `GT` 要求严格更大，`LT` 要求严格更小，而当前无过期时 `LT` 视为无限大所以总成立。
fn expire_condition_allows(
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    key: &[u8],
    flags: ExpireFlags,
    expires_at: i64,
) -> Result<bool, CommandError> {
    if !flags.any() {
        return Ok(true);
    }
    let current =
        match executor.execute(None, KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::GetExpiry }) {
            Ok(KeyValueResult::Expiry { expires_at, .. }) => expires_at,
            Ok(_) => {
                return Err(CommandError::Failed("EXPIRE executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
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
    Ok(allowed)
}

/// `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]`。
///
/// 游标是「已检查键数」的十进制下标：每次返回 `COUNT` 个键（不足则到末尾），返回 0
/// 表示迭代结束。所有值类型都参与扫描，键按字节序合并后去重，因此一次调用不保证
/// 返回全部键，但持续回传游标直到 0 能覆盖当前键空间。`TYPE` 只筛出该类型的键；
/// 真机对未知类型名不回错误，只是没有任何键能命中，这里同样返回空结果。
fn handle_scan(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
) -> Result<Reply, CommandError> {
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    if request.arguments.len() < 2 {
        return Err(CommandError::Failed("wrong number of arguments for 'scan' command".to_owned()));
    }
    let cursor_text = match std::str::from_utf8(request.arguments[1]) {
        Ok(text) if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => text,
        _ => {
            return Err(CommandError::Failed("invalid cursor".to_owned()));
        }
    };
    let Ok(cursor) = cursor_text.parse::<u64>()
    else {
        return Err(CommandError::Failed("invalid cursor".to_owned()));
    };
    let mut pattern: Option<Vec<u8>> = None;
    let mut kind_filter: Option<Option<KeyKind>> = None;
    let mut count = 10usize;
    let mut index = 2;
    while index < request.arguments.len() {
        if index + 1 >= request.arguments.len() {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        if request.arguments[index].eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(request.arguments[index + 1].to_vec());
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"COUNT") {
            // 真机把越界的 COUNT 报成整数错误，把非正数报成语法错误，两者不能混用。
            let Ok(value) = std::str::from_utf8(request.arguments[index + 1]).unwrap_or("").parse::<i64>()
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            if value <= 0 {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            count = usize::try_from(value).unwrap_or(usize::MAX);
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"TYPE") {
            // 未知类型名在真机上不是错误，只是筛不出任何键，所以这里保留 `None` 并继续过滤。
            kind_filter = Some(KeyKind::from_name(request.arguments[index + 1]));
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
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
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        }
    }
    keys.sort();
    keys.dedup();
    let start = cursor as usize;
    if start >= keys.len() {
        return Ok(Reply::array(vec![Reply::bulk(b"0".to_vec()), Reply::array(vec![])]));
    }
    let end = start.saturating_add(count).min(keys.len());
    let next = if end >= keys.len() { "0".to_string() } else { end.to_string() };
    let matched: Vec<Reply> = keys[start..end]
        .iter()
        .filter(|key| pattern.as_ref().map_or(true, |pattern| glob_match(pattern, key)))
        .map(|key| Reply::bulk(key.clone()))
        .collect();
    Ok(Reply::array(vec![Reply::bulk(next.into_bytes()), Reply::array(matched)]))
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
fn handle_copy(request: &Request<'_>, executor: &dyn KeyValueExecutor, namespace: &Namespace) -> Result<Reply, CommandError> {
    if request.arguments.len() < 3 {
        return Err(CommandError::Failed("wrong number of arguments for 'copy' command".to_owned()));
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
                return Err(CommandError::Failed(message.to_owned()));
            };
            destination_database = Some(database);
            index += 2;
        }
        else if request.arguments[index].eq_ignore_ascii_case(b"REPLACE") {
            replace = true;
            index += 1;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    let destination_base = match destination_database {
        Some(database) => database_namespace(namespace, u32::from(database)),
        None => namespace.clone(),
    };
    if destination_base == *namespace && source == destination {
        return Err(CommandError::Failed("source and destination objects are the same".to_owned()));
    }
    let source_kind = match collections::find_kind(executor, namespace, source) {
        Ok(Some(kind)) => kind,
        Ok(None) => {
            return Ok(Reply::integer(0));
        }
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    };
    let destination_exists = match collections::find_kind(executor, &destination_base, destination) {
        Ok(exists) => exists.is_some(),
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    };
    if destination_exists && !replace {
        return Ok(Reply::integer(0));
    }
    let source_namespace = source_kind.namespace(namespace);
    let value = match executor
        .execute(None, KeyValueCommand { namespace: source_namespace.clone(), key: source.to_vec(), action: KeyValueAction::Get })
    {
        Ok(KeyValueResult::Get(Some(value))) => value,
        Ok(KeyValueResult::Get(None)) => {
            return Ok(Reply::integer(0));
        }
        Ok(_) => return Err(CommandError::Failed("COPY executor returned unexpected result".to_owned())),
        Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
    };
    let expires_at = match executor
        .execute(None, KeyValueCommand { namespace: source_namespace, key: source.to_vec(), action: KeyValueAction::GetExpiry })
    {
        Ok(KeyValueResult::Expiry { expires_at, .. }) => expires_at,
        Ok(_) => return Err(CommandError::Failed("COPY executor returned unexpected result".to_owned())),
        Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
    };
    for kind in KeyKind::ALL {
        let command =
            KeyValueCommand { namespace: kind.namespace(&destination_base), key: destination.to_vec(), action: KeyValueAction::Delete };
        if let Err(error) = executor.execute(None, command) {
            return Err(CommandError::Failed(error.to_string().to_owned()));
        }
    }
    let destination_namespace = source_kind.namespace(&destination_base);
    match executor.execute(
        None,
        KeyValueCommand { namespace: destination_namespace.clone(), key: destination.to_vec(), action: KeyValueAction::Put(value) },
    ) {
        Ok(KeyValueResult::Put { .. }) => {}
        Ok(_) => return Err(CommandError::Failed("COPY executor returned unexpected result".to_owned())),
        Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
    }
    if let Some(expires_at) = expires_at {
        let command =
            KeyValueCommand { namespace: destination_namespace, key: destination.to_vec(), action: KeyValueAction::SetExpiry { expires_at } };
        if let Err(error) = executor.execute(None, command) {
            return Err(CommandError::Failed(error.to_string().to_owned()));
        }
    }
    Ok(Reply::integer(1))
}

/// `OBJECT IDLETIME` / `OBJECT REFCOUNT` / `OBJECT FREQ`.
///
/// 三个子命令先按真机的 `LOOKUP_NOTOUCH` 口径查键：缺键回 null（RESP2 `$-1`、RESP3 `_`），
/// 查询本身不刷新访问计时。这里用未包装的执行器，因此 `OBJECT IDLETIME` 不会把空闲
/// 时间清零。
fn handle_object(
    request: &Request<'_>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let count = request.arguments.len();
    let subcommand = request.arguments.get(1).copied().unwrap_or(b"");
    if count < 2 {
        return Err(CommandError::Failed("wrong number of arguments for 'object' command".to_owned()));
    }
    let known = subcommand.eq_ignore_ascii_case(b"IDLETIME")
        || subcommand.eq_ignore_ascii_case(b"REFCOUNT")
        || subcommand.eq_ignore_ascii_case(b"FREQ")
        || subcommand.eq_ignore_ascii_case(b"ENCODING");
    if !known {
        return Err(CommandError::Failed(format!("unknown subcommand '{}'. Try OBJECT HELP.", String::from_utf8_lossy(subcommand)).to_owned()));
    }
    if count != 3 {
        return Err(CommandError::Failed(format!(
            "wrong number of arguments for 'object|{}' command",
            String::from_utf8_lossy(subcommand).to_ascii_lowercase()
        )));
    }
    let Some(executor) = probe
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    let key = request.arguments[2];
    // 集合键也参与 `OBJECT`：先解析逻辑键落在哪个类型命名空间，再按该命名空间计时。
    let (kind, target) = match resolve_kind(executor, namespace, key)? {
        None => return Ok(Reply::nil()),
        Some(kind) => (kind, kind.namespace(namespace)),
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
                return Err(CommandError::Failed("OBJECT ENCODING executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        let Some(value) = value
        else {
            return Ok(Reply::nil());
        };
        return Ok(Reply::bulk(object_encoding(kind, key, &value).as_bytes().to_vec()));
    }
    if subcommand.eq_ignore_ascii_case(b"IDLETIME") {
        // 纯内存计时：不写 `.yykv`、不升格式版本，重启后从零开始，与真机 RDB 的行为一致。
        // 无连接计时的调用方（绑定原语）看到的是一个刚建立的连接，空闲时间从 0 起算。
        let idle = tracker.map(|tracker| tracker.idle_seconds(&target, key)).unwrap_or(0);
        Ok(Reply::integer(idle as i64))
    }
    else if subcommand.eq_ignore_ascii_case(b"REFCOUNT") {
        // 这是由存储结构推出的结果，不是写死的常量：每条记录在 `Record` 里独占自己的
        // 值字节，既没有共享对象表也没有引用计数表，所以每个存在的键恰好只有一个引用。
        // 引入对象共享会改变表示与落盘口径，故不实现。
        Ok(Reply::integer(1))
    }
    else {
        // 网关没有淘汰策略配置面，恒为 LRU 口径：不做频率追踪，也不编造一个假计数。
        Err(CommandError::Failed(
            "An LFU maxmemory policy is not selected, access frequency not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust.".to_owned(),
        ))
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

/// 服务键空间族，返回 `None` 表示命令不属于该族。
pub fn handle_keyspace(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Option<Reply>, CommandError> {
    let command = request.arguments[0];
    if !is_keyspace_command(command) {
        return Ok(None);
    }
    let count = request.arguments.len();
    if command.eq_ignore_ascii_case(b"TYPE") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'type' command".to_owned()));
        }
        let key = request.arguments[1];
        let name = match resolve_kind(executor, namespace, key)? {
            Some(kind) => kind.name(),
            None => "none",
        };
        return Ok(Some(Reply::status(name)));
    }
    else if command.eq_ignore_ascii_case(b"KEYS") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'keys' command".to_owned()));
        }
        match collect_keys(executor, namespace) {
            Ok(keys) => {
                let matched: Vec<Reply> = keys.into_iter().filter(|key| glob_match(request.arguments[1], key)).map(Reply::bulk).collect();
                return Ok(Some(Reply::array(matched)));
            }
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
    }
    else if command.eq_ignore_ascii_case(b"SCAN") {
        return Ok(Some(handle_scan(request, executor, namespace)?));
    }
    else {
        return Ok(Some(handle_object(request, probe, tracker, namespace, protocol)?));
    }
}

/// Serves the string and key family, returning `None` when the command belongs to a
/// different family so the caller keeps dispatching.
pub fn handle_string(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: Option<&KeyAccessTracker>,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Option<Reply>, CommandError> {
    let command = request.arguments[0];
    let count = request.arguments.len();
    if !is_string_command(command) {
        return Ok(None);
    }
    // 键空间族的四个命令曾经散落在这条长链里，抽成独立族后仍从这里分派，
    // 因此旧调用方（监听器与字符串绑定原语）行为不变。
    if is_keyspace_command(command) {
        return handle_keyspace(request, executor, probe, tracker, namespace, protocol);
    }
    if command.eq_ignore_ascii_case(b"EXPIRE") || command.eq_ignore_ascii_case(b"PEXPIRE") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'expire' command".to_owned()));
        }
        let flags = match parse_expire_flags(&request.arguments) {
            Ok(flags) => flags,
            Err(error) => {
                return Err(expire_flag_error(error));
            }
        };
        let key = request.arguments[1];
        let delta = match parse_integer(request.arguments[2]) {
            Some(v) => v,
            None => {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            }
        };
        let target = match resolve_kind(executor, namespace, key)? {
            None => {
                return Ok(Some(Reply::integer(0)));
            }
            Some(kind) => kind.namespace(namespace),
        };
        let unit_ms: i64 = if command.eq_ignore_ascii_case(b"PEXPIRE") { 1 } else { 1000 };
        let Some(offset) = delta.checked_mul(unit_ms)
        else {
            return Err(CommandError::Failed("invalid expire time in 'expire' command".to_owned()));
        };
        let now = now_unix_ms();
        let expires_at = now.saturating_add(offset);
        // 条件先于删除生效：`GT` 在相对过去的截止时间上会拒绝，键必须被保留。
        if !expire_condition_allows(executor, &target, key, flags, expires_at)? {
            return Ok(Some(Reply::integer(0)));
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
                    return Err(CommandError::Failed("EXPIRE executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            };
            return Ok(Some(Reply::integer(i64::from(removed))));
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
                return Err(CommandError::Failed("EXPIRE executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        return Ok(Some(Reply::integer(i64::from(applied))));
    }
    else if command.eq_ignore_ascii_case(b"EXPIREAT") || command.eq_ignore_ascii_case(b"PEXPIREAT") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'expireat' command".to_owned()));
        }
        let flags = match parse_expire_flags(&request.arguments) {
            Ok(flags) => flags,
            Err(error) => {
                return Err(expire_flag_error(error));
            }
        };
        let key = request.arguments[1];
        let at = match parse_integer(request.arguments[2]) {
            Some(v) => v,
            None => {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            }
        };
        let target = match resolve_kind(executor, namespace, key)? {
            None => {
                return Ok(Some(Reply::integer(0)));
            }
            Some(kind) => kind.namespace(namespace),
        };
        let unit_ms: i64 = if command.eq_ignore_ascii_case(b"PEXPIREAT") { 1 } else { 1000 };
        let Some(expires_at) = at.checked_mul(unit_ms)
        else {
            return Err(CommandError::Failed("invalid expire time in 'expireat' command".to_owned()));
        };
        let now = now_unix_ms();
        // 条件先于删除生效，理由同 `EXPIRE`。
        if !expire_condition_allows(executor, &target, key, flags, expires_at)? {
            return Ok(Some(Reply::integer(0)));
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
                    return Err(CommandError::Failed("EXPIREAT executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            };
            return Ok(Some(Reply::integer(i64::from(removed))));
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
                return Err(CommandError::Failed("EXPIREAT executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        return Ok(Some(Reply::integer(i64::from(applied))));
    }
    else if command.eq_ignore_ascii_case(b"MGET") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        match bind_mget(&request.arguments, namespace.0.as_str()) {
            Ok(commands) => {
                let commands: Vec<KeyValueCommand> = commands.into_iter().map(to_kv_command).collect();
                // Expired records are filtered by the KV layer itself, so the batch read
                // already reports them as absent.
                match executor.execute_read_batch(None, &commands) {
                    Ok(results) => {
                        if results.len() != commands.len() || results.iter().any(|result| !matches!(result, KeyValueResult::Get(_))) {
                            return Err(CommandError::Failed("MGET executor returned an invalid read batch".to_owned()));
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
                                    return Err(CommandError::WrongType);
                                }
                                Err(message) => {
                                    return Err(CommandError::Failed(message.to_owned()));
                                }
                            }
                        }
                        let items: Vec<Reply> = results
                            .into_iter()
                            .map(|result| match result {
                                KeyValueResult::Get(Some(value)) => Reply::bulk(value),
                                KeyValueResult::Get(None) => Reply::nil(),
                                _ => unreachable!(),
                            })
                            .collect();
                        return Ok(Some(Reply::array(items)));
                    }
                    Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
                }
            }
            Err(BindError::WrongArity) => return Err(CommandError::Failed("wrong number of arguments".to_owned())),
            Err(BindError::EmptyCommand) => return Err(CommandError::Failed("empty command".to_owned())),
            Err(BindError::UnsupportedCommand) => return Err(CommandError::Failed("unsupported command".to_owned())),
            _ => return Err(CommandError::Failed("invalid MGET command".to_owned())),
        }
    }
    else if command.eq_ignore_ascii_case(b"EXISTS") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'exists' command".to_owned()));
        }
        let mut present: i64 = 0;
        for key in &request.arguments[1..] {
            match resolve_kind(executor, namespace, key)? {
                Some(_) => present += 1,
                None => {}
            }
        }
        return Ok(Some(Reply::integer(present)));
    }
    else if command.eq_ignore_ascii_case(b"TTL") || command.eq_ignore_ascii_case(b"PTTL") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'ttl' command".to_owned()));
        }
        let key = request.arguments[1];
        let target = match resolve_kind(executor, namespace, key)? {
            None => {
                return Ok(Some(Reply::integer(-2)));
            }
            Some(kind) => kind.namespace(namespace),
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
                return Err(CommandError::Failed("TTL executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
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
        return Ok(Some(Reply::integer(value)));
    }
    else if command.eq_ignore_ascii_case(b"STRLEN") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'strlen' command".to_owned()));
        }
        let key = request.arguments[1];
        match resolve_kind(executor, namespace, key)? {
            Some(KeyKind::String) | None => {}
            Some(_) => {
                return Err(CommandError::WrongType);
            }
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
                return Err(CommandError::Failed("STRLEN executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        return Ok(Some(Reply::integer(value)));
    }
    else if command.eq_ignore_ascii_case(b"APPEND") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'append' command".to_owned()));
        }
        let key = request.arguments[1];
        let append = request.arguments[2];
        match resolve_kind(executor, namespace, key)? {
            Some(KeyKind::String) | None => {}
            Some(_) => {
                return Err(CommandError::WrongType);
            }
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
                return Err(CommandError::Failed("APPEND executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        let mut merged = current;
        merged.extend_from_slice(append);
        let new_len = merged.len() as i64;
        // APPEND keeps the TTL of the appended key. When the key is absent the record
        // simply has no expiry, so one expiry-preserving action covers both cases.
        return match executor.execute(
            None,
            database_query::cluster_kv::KeyValueCommand {
                namespace: namespace.clone(),
                key: key.to_vec(),
                action: database_query::cluster_kv::KeyValueAction::PutKeepingExpiry(merged),
            },
        ) {
            Ok(KeyValueResult::Put { .. }) => Ok(Some(Reply::integer(new_len))),
            Ok(_) => {
                return Err(CommandError::Failed("APPEND executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"GETRANGE") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 4 {
            return Err(CommandError::Failed("wrong number of arguments for 'getrange' command".to_owned()));
        }
        let key = request.arguments[1];
        let (Some(start), Some(stop)) = (collections::parse_index(request.arguments[2]), collections::parse_index(request.arguments[3]))
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        ensure_string(executor, namespace, key)?;
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value,
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        match value.and_then(|bytes| normalized_range(start, stop, bytes.len()).map(|(begin, end)| bytes[begin..=end].to_vec())) {
            Some(slice) => return Ok(Some(Reply::bulk(slice.to_vec()))),
            None => return Ok(Some(Reply::bulk(b"".to_vec()))),
        }
    }
    else if command.eq_ignore_ascii_case(b"SETRANGE") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 4 {
            return Err(CommandError::Failed("wrong number of arguments for 'setrange' command".to_owned()));
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
                return Err(CommandError::Failed("offset is out of range".to_owned()));
            }
            None => {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            }
        };
        ensure_string(executor, namespace, key)?;
        let current = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        // 空值只回当前长度，不创建键、也不改动已有内容。
        if replacement.is_empty() {
            return Ok(Some(Reply::integer(current.len() as i64)));
        }
        let end = offset.saturating_add(replacement.len() as u64);
        if end > MAX_STRING_BYTES {
            return Err(CommandError::Failed("string exceeds maximum allowed size (proto-max-bulk-len)".to_owned()));
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
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Some(Reply::integer(new_len as i64)));
    }
    else if command.eq_ignore_ascii_case(b"INCRBYFLOAT") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'incrbyfloat' command".to_owned()));
        }
        let key = request.arguments[1];
        let increment = match std::str::from_utf8(request.arguments[2]).ok().and_then(|text| text.parse::<f64>().ok()) {
            Some(increment) => increment,
            None => {
                return Err(CommandError::Failed("value is not a valid float".to_owned()));
            }
        };
        // 真机解析 `nan` 直接失败（字符串本身就是非法浮点），而 `+inf` / `-inf`
        // 能解析却在结果阶段被「非有限」规则拒绝。
        if increment.is_nan() {
            return Err(CommandError::Failed("value is not a valid float".to_owned()));
        }
        if increment.is_infinite() {
            return Err(CommandError::Failed("increment would produce NaN or Infinity".to_owned()));
        }
        ensure_string(executor, namespace, key)?;
        let current = match load_string(executor, namespace, key) {
            Ok(None) => 0.0,
            Ok(Some(bytes)) => match std::str::from_utf8(&bytes).ok().and_then(|text| text.parse::<f64>().ok()) {
                // 存量值只拒绝 NaN 与非数字写法；inf 交给结果阶段报错。
                Some(value) if !value.is_nan() => value,
                _ => {
                    return Err(CommandError::Failed("value is not a valid float".to_owned()));
                }
            },
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        let result = current + increment;
        if !result.is_finite() {
            return Err(CommandError::Failed("increment would produce NaN or Infinity".to_owned()));
        }
        let formatted = format_float(result);
        if let Err(message) = store_string(executor, namespace, key, formatted.clone().into_bytes()) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Some(Reply::bulk(formatted.into_bytes())));
    }
    else if command.eq_ignore_ascii_case(b"SETBIT") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 4 {
            return Err(CommandError::Failed("wrong number of arguments for 'setbit' command".to_owned()));
        }
        let key = request.arguments[1];
        let Some(offset) = parse_offset(request.arguments[2]).filter(|offset| *offset <= u32::MAX as u64)
        else {
            return Err(CommandError::Failed("bit offset is not an integer or out of range".to_owned()));
        };
        let bit = match request.arguments[3] {
            b"0" => false,
            b"1" => true,
            _ => {
                return Err(CommandError::Failed("bit is not an integer or out of range".to_owned()));
            }
        };
        ensure_string(executor, namespace, key)?;
        let mut value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
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
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Some(Reply::integer(i64::from(previous))));
    }
    else if command.eq_ignore_ascii_case(b"GETBIT") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'getbit' command".to_owned()));
        }
        let key = request.arguments[1];
        let Some(offset) = parse_offset(request.arguments[2]).filter(|offset| *offset <= u32::MAX as u64)
        else {
            return Err(CommandError::Failed("bit offset is not an integer or out of range".to_owned()));
        };
        ensure_string(executor, namespace, key)?;
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        let byte = (offset / 8) as usize;
        let bit = value.get(byte).map_or(0, |byte| (byte >> (7 - (offset % 8) as u32)) & 1);
        return Ok(Some(Reply::integer(i64::from(bit))));
    }
    else if command.eq_ignore_ascii_case(b"BITCOUNT") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if !matches!(count, 2 | 4 | 5) {
            return Err(CommandError::Failed("wrong number of arguments for 'bitcount' command".to_owned()));
        }
        let key = request.arguments[1];
        let bit_unit = match count {
            5 if request.arguments[4].eq_ignore_ascii_case(b"BIT") => true,
            5 if request.arguments[4].eq_ignore_ascii_case(b"BYTE") => false,
            5 => {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            _ => false,
        };
        let range = if count == 2 {
            None
        }
        else {
            let (Some(start), Some(stop)) = (collections::parse_index(request.arguments[2]), collections::parse_index(request.arguments[3]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            Some((start, stop))
        };
        ensure_string(executor, namespace, key)?;
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value.unwrap_or_default(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
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
        return Ok(Some(Reply::integer(total)));
    }
    else if command.eq_ignore_ascii_case(b"BITPOS") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if !(3..=6).contains(&count) {
            return Err(CommandError::Failed("wrong number of arguments for 'bitpos' command".to_owned()));
        }
        let key = request.arguments[1];
        let target: u8 = match request.arguments[2] {
            b"0" => 0,
            b"1" => 1,
            _ => {
                return Err(CommandError::Failed("The bit argument must be 1 or 0.".to_owned()));
            }
        };
        let bit_unit = match count {
            6 if request.arguments[5].eq_ignore_ascii_case(b"BIT") => true,
            6 if request.arguments[5].eq_ignore_ascii_case(b"BYTE") => false,
            6 => {
                return Err(CommandError::Failed("syntax error".to_owned()));
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
                    return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
                }
            },
        };
        let parsed_stop = match stop {
            None => None,
            Some(raw) => match collections::parse_index(raw) {
                Some(index) => Some(index),
                None => {
                    return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
                }
            },
        };
        ensure_string(executor, namespace, key)?;
        let value = match load_string(executor, namespace, key) {
            Ok(value) => value,
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        // 键不存在时真机把它视为无限长的 0 串，忽略范围：找 0 回 0，找 1 回 -1。
        let Some(value) = value
        else {
            let answer = if target == 0 { 0 } else { -1 };
            return Ok(Some(Reply::integer(answer)));
        };
        // 真机在归一化之前先比原始负索引：两个负值且起点大于终点即空区间。
        if let (Some(start), Some(stop)) = (parsed_start, parsed_stop) {
            if start < 0 && stop < 0 && start > stop {
                return Ok(Some(Reply::integer(-1)));
            }
        }
        let total_bits = (value.len() * 8) as i64;
        let units = if bit_unit { total_bits } else { value.len() as i64 };
        let begin_unit = parsed_start.map_or(0, |start| normalize_unit(start, units));
        let end_unit = parsed_stop.map_or(units - 1, |stop| normalize_unit(stop, units));
        if begin_unit > end_unit {
            return Ok(Some(Reply::integer(-1)));
        }
        let (begin, end) = if bit_unit { (begin_unit, end_unit) } else { (begin_unit * 8, end_unit * 8 + 7) };
        match find_bit(&value, begin as usize, end as usize, target) {
            Some(bit) => return Ok(Some(Reply::integer(bit as i64))),
            None => {
                // 没显式给终点且找 0 时，末尾之后的第一个位就是答案。
                let answer = if target == 0 && !end_given { total_bits } else { -1 };
                return Ok(Some(Reply::integer(answer)));
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"SET") {
        return Ok(Some(handle_set(request, executor, namespace, protocol)?));
    }
    else if command.eq_ignore_ascii_case(b"PERSIST") || command.eq_ignore_ascii_case(b"SETEX") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if command.eq_ignore_ascii_case(b"PERSIST") {
            if count != 2 {
                return Err(CommandError::Failed("wrong number of arguments for 'persist' command".to_owned()));
            }
            let key = request.arguments[1];
            let target = match resolve_kind(executor, namespace, key)? {
                None => {
                    return Ok(Some(Reply::integer(0)));
                }
                Some(kind) => kind.namespace(namespace),
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
                    return Err(CommandError::Failed("PERSIST executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            };
            return Ok(Some(Reply::integer(i64::from(applied))));
        }
        else {
            if count != 4 {
                return Err(CommandError::Failed("wrong number of arguments for 'setex' command".to_owned()));
            }
            let Some(seconds) = parse_integer(request.arguments[2])
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            if seconds <= 0 {
                return Err(CommandError::Failed("invalid expire time in 'setex' command".to_owned()));
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
                        return Err(CommandError::Failed(message.to_owned()));
                    }
                    return Ok(Some(Reply::ok()));
                }
                Ok(_) => {
                    return Err(CommandError::Failed("SETEX executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"GETEX") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'getex' command".to_owned()));
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
                return Err(CommandError::Failed("syntax error".to_owned()));
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
                    return Err(CommandError::Failed("syntax error".to_owned()));
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
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
        }
        let key = request.arguments[1];
        // GETEX is a string command: a hash/list key is WRONGTYPE rather than nil.
        match resolve_kind(executor, namespace, key)? {
            Some(KeyKind::String) | None => {}
            Some(_) => {
                return Err(CommandError::WrongType);
            }
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
                return Err(CommandError::Failed("GETEX executor returned unexpected result".to_owned()));
            }
            Err(error) => {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        };
        // An absent (or expired) key returns nil and nothing is touched, so the expiry
        // argument is deliberately left unparsed in that branch.
        let Some(value) = value
        else {
            return Ok(Some(Reply::nil()));
        };
        if let Some(spec) = spec {
            let action = match spec {
                ExpirySpec::Persist => database_query::cluster_kv::KeyValueAction::ClearExpiry,
                ExpirySpec::Seconds(raw) | ExpirySpec::Milliseconds(raw) | ExpirySpec::SecondsAt(raw) | ExpirySpec::MillisecondsAt(raw) => {
                    let Some(value) = parse_integer(raw)
                    else {
                        return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
                    };
                    if value <= 0 {
                        return Err(CommandError::Failed("invalid expire time in 'getex' command".to_owned()));
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
                        return Err(CommandError::Failed("invalid expire time in 'getex' command".to_owned()));
                    };
                    database_query::cluster_kv::KeyValueAction::SetExpiry { expires_at }
                }
            };
            if let Err(error) =
                executor.execute(None, database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action })
            {
                return Err(CommandError::Failed(error.to_string().to_owned()));
            }
        }
        return Ok(Some(Reply::bulk(value)));
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
            return Err(CommandError::Failed(format!("wrong number of arguments for '{name}' command")));
        }
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        let source = request.arguments[1];
        let destination = request.arguments[2];
        let kind = match collections::find_kind(executor, namespace, source) {
            Ok(Some(kind)) => kind,
            Ok(None) => {
                return Err(CommandError::Failed("no such key".to_owned()));
            }
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        if source == destination {
            if rename_nx {
                return Ok(Some(Reply::integer(0)));
            }
            return Ok(Some(Reply::ok()));
        }
        let destination_exists = match collections::find_kind(executor, namespace, destination) {
            Ok(exists) => exists.is_some(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        };
        if rename_nx && destination_exists {
            return Ok(Some(Reply::integer(0)));
        }
        if destination_exists {
            // 覆盖目标时先清掉它可能存在的其它类型表示，避免一次逻辑键出现两份值。
            for other in KeyKind::ALL {
                let command =
                    KeyValueCommand { namespace: other.namespace(namespace), key: destination.to_vec(), action: KeyValueAction::Delete };
                if let Err(error) = executor.execute(None, command) {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            }
        }
        let action = KeyValueAction::Rename { destination: destination.to_vec(), if_absent: false };
        let command = KeyValueCommand { namespace: kind.namespace(namespace), key: source.to_vec(), action };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Rename { source_present: true, .. }) => {
                if rename_nx {
                    return Ok(Some(Reply::integer(1)));
                }
                else {
                    return Ok(Some(Reply::ok()));
                }
            }
            Ok(KeyValueResult::Rename { source_present: false, .. }) => return Err(CommandError::Failed("no such key".to_owned())),
            Ok(_) => return Err(CommandError::Failed("RENAME executor returned unexpected result".to_owned())),
            Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
        }
    }
    else if command.eq_ignore_ascii_case(b"DEL") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'del' command".to_owned()));
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
                        return Err(CommandError::Failed("DEL executor returned unexpected result".to_owned()));
                    }
                    Err(error) => {
                        return Err(CommandError::Failed(error.to_string().to_owned()));
                    }
                }
            }
            if deleted {
                removed += 1;
            }
        }
        return Ok(Some(Reply::integer(removed)));
    }
    else if command.eq_ignore_ascii_case(b"RANDOMKEY") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 1 {
            return Err(CommandError::Failed("wrong number of arguments for 'randomkey' command".to_owned()));
        }
        return match collect_keys(executor, namespace) {
            Ok(keys) if keys.is_empty() => Ok(Some(Reply::nil())),
            Ok(keys) => {
                let index = random_index(keys.len());
                Ok(Some(Reply::bulk(keys[index].clone())))
            }
            Err(message) => Err(CommandError::Failed(message.to_owned())),
        };
    }
    else if command.eq_ignore_ascii_case(b"DBSIZE") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 1 {
            return Err(CommandError::Failed("wrong number of arguments for 'dbsize' command".to_owned()));
        }
        return match collect_keys(executor, namespace) {
            Ok(keys) => Ok(Some(Reply::integer(keys.len() as i64))),
            Err(message) => Err(CommandError::Failed(message.to_owned())),
        };
    }
    else if command.eq_ignore_ascii_case(b"FLUSHALL") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count > 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'flushall' command".to_owned()));
        }
        if count == 2 && !request.arguments[1].eq_ignore_ascii_case(b"ASYNC") && !request.arguments[1].eq_ignore_ascii_case(b"SYNC") {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let bases = (0..16u32).map(|database| database_namespace(namespace, database));
        return match flush_databases(executor, bases) {
            Some(message) => Err(CommandError::Failed(message.to_owned())),
            None => Ok(Some(Reply::ok())),
        };
    }
    else if command.eq_ignore_ascii_case(b"FLUSHDB") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count > 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'flushdb' command".to_owned()));
        }
        if count == 2 && !request.arguments[1].eq_ignore_ascii_case(b"ASYNC") && !request.arguments[1].eq_ignore_ascii_case(b"SYNC") {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        return match flush_databases(executor, [namespace.clone()]) {
            Some(message) => Err(CommandError::Failed(message.to_owned())),
            None => Ok(Some(Reply::ok())),
        };
    }
    else if command.eq_ignore_ascii_case(b"COPY") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        return Ok(Some(handle_copy(request, executor, namespace)?));
    }
    else if command.eq_ignore_ascii_case(b"EXPIRETIME") || command.eq_ignore_ascii_case(b"PEXPIRETIME") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 2 {
            let name = if command.eq_ignore_ascii_case(b"PEXPIRETIME") { "pexpiretime" } else { "expiretime" };
            return Err(CommandError::Failed(format!("wrong number of arguments for '{name}' command").to_owned()));
        }
        let key = request.arguments[1];
        let target = match resolve_kind(executor, namespace, key)? {
            None => {
                return Ok(Some(Reply::integer(-2)));
            }
            Some(kind) => kind.namespace(namespace),
        };
        let (present, expires_at) =
            match executor.execute(None, KeyValueCommand { namespace: target, key: key.to_vec(), action: KeyValueAction::GetExpiry }) {
                Ok(KeyValueResult::Expiry { present, expires_at }) => (present, expires_at),
                Ok(_) => {
                    return Err(CommandError::Failed("EXPIRETIME executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
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
        return Ok(Some(Reply::integer(value)));
    }
    else if command.eq_ignore_ascii_case(b"SETNX") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'setnx' command".to_owned()));
        }
        let key = request.arguments[1];
        return match resolve_kind(executor, namespace, key)? {
            // 任意类型的旧键都算「已存在」，真机不会因为类型不同而报错。
            Some(_) => Ok(Some(Reply::integer(0))),
            None => {
                let action = database_query::cluster_kv::KeyValueAction::PutIfAbsent(request.arguments[2].to_vec());
                let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action };
                match executor.execute(None, command) {
                    Ok(KeyValueResult::PutIfAbsent { revision }) => Ok(Some(Reply::integer(i64::from(revision.is_some())))),
                    Ok(_) => Err(CommandError::Failed("SETNX executor returned unexpected result".to_owned())),
                    Err(error) => Err(CommandError::Failed(error.to_string().to_owned())),
                }
            }
        }
    }
    else if command.eq_ignore_ascii_case(b"PSETEX") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count != 4 {
            return Err(CommandError::Failed("wrong number of arguments for 'psetex' command".to_owned()));
        }
        let Some(milliseconds) = parse_integer(request.arguments[2])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        if milliseconds <= 0 {
            return Err(CommandError::Failed("invalid expire time in 'psetex' command".to_owned()));
        }
        let expires_at = now_unix_ms().saturating_add(milliseconds);
        let action = database_query::cluster_kv::KeyValueAction::PutExpiring { value: request.arguments[3].to_vec(), expires_at };
        let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: request.arguments[1].to_vec(), action };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Put { .. }) => {
                if let Err(message) = clear_other_kinds(executor, namespace, request.arguments[1]) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
                return Ok(Some(Reply::ok()));
            }
            Ok(_) => return Err(CommandError::Failed("PSETEX executor returned unexpected result".to_owned())),
            Err(error) => return Err(CommandError::Failed(error.to_string().to_owned())),
        }
    }
    else if command.eq_ignore_ascii_case(b"MSET") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        if count < 3 || count % 2 == 0 {
            return Err(CommandError::Failed("wrong number of arguments for 'mset' command".to_owned()));
        }
        for pair in request.arguments[1..].chunks(2) {
            let key = pair[0];
            let action = database_query::cluster_kv::KeyValueAction::Put(pair[1].to_vec());
            let command = database_query::cluster_kv::KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action };
            match executor.execute(None, command) {
                Ok(KeyValueResult::Put { .. }) => {
                    if let Err(message) = clear_other_kinds(executor, namespace, key) {
                        return Err(CommandError::Failed(message.to_owned()));
                    }
                }
                Ok(_) => {
                    return Err(CommandError::Failed("MSET executor returned unexpected result".to_owned()));
                }
                Err(error) => {
                    return Err(CommandError::Failed(error.to_string().to_owned()));
                }
            }
        }
        return Ok(Some(Reply::ok()));
    }
    else if command.eq_ignore_ascii_case(b"BITFIELD") || command.eq_ignore_ascii_case(b"BITFIELD_RO") {
        let Some(executor) = executor
        else {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        };
        return collections::handle_bitfield(executor, namespace, &request.arguments, command.eq_ignore_ascii_case(b"BITFIELD_RO"), protocol)
            .map(Some);
    }
    else if let Some(executor) = executor {
        // --- Generic bind()-backed commands (GET, INCR, DECR, GETDEL, GETSET).
        // Expiry semantics are enforced by the KV layer: expired records read as absent
        // and each action carries its own expiry behaviour.
        return match bind(&request.arguments, namespace.0.as_str()) {
            Ok(command) => {
                // 这些命令只对字符串有效：hash / list 键在真机上回 WRONGTYPE 而不是 nil。
                // 绑定成功后再探测，保证参数错误优先于类型错误。
                if matches!(command.action, BoundAction::Get | BoundAction::GetDelete | BoundAction::GetSet(_) | BoundAction::IncrementBy(_)) {
                    match resolve_kind(executor, namespace, &command.key)? {
                        Some(KeyKind::String) | None => {}
                        Some(_) => {
                            return Err(CommandError::WrongType);
                        }
                    }
                }
                match executor.execute(None, to_kv_command(command)) {
                    Ok(result) => {
                        match result {
                            KeyValueResult::Get(Some(value)) => Ok(Some(Reply::bulk(value))),
                            KeyValueResult::Get(None) => Ok(Some(Reply::nil())),
                            KeyValueResult::Exists(exists) => Ok(Some(Reply::integer(i64::from(exists)))),
                            KeyValueResult::GetDelete(Some(value)) => Ok(Some(Reply::bulk(value))),
                            KeyValueResult::GetDelete(None) => Ok(Some(Reply::nil())),
                            KeyValueResult::GetSet(Some(value)) => Ok(Some(Reply::bulk(value))),
                            KeyValueResult::GetSet(None) => Ok(Some(Reply::nil())),
                            KeyValueResult::Put { .. } => Ok(Some(Reply::ok())),
                            KeyValueResult::PutIfAbsent { revision: Some(_) } => Ok(Some(Reply::ok())),
                            KeyValueResult::PutIfAbsent { revision: None } => Ok(Some(Reply::nil())),
                            KeyValueResult::PutIfPresent { revision: Some(_) } => Ok(Some(Reply::ok())),
                            KeyValueResult::PutIfPresent { revision: None } => Ok(Some(Reply::nil())),
                            KeyValueResult::Increment { value } => Ok(Some(Reply::integer(value))),
                            KeyValueResult::Delete { removed } => Ok(Some(Reply::integer(i64::from(removed)))),
                            // Expiry, revision and rename results only surface through their
                            // dedicated paths, never through the generic bind() commands.
                            KeyValueResult::Expiry { .. }
                            | KeyValueResult::ExpiryChanged { .. }
                            | KeyValueResult::Revision(_)
                            | KeyValueResult::Rename { .. } => {
                                return Err(CommandError::Failed("unexpected result from bind command".to_owned()));
                            }
                        }
                    }
                    Err(error) => Err(CommandError::Failed(error.to_string().to_owned())),
                }
            }
            Err(BindError::WrongArity) => Err(CommandError::Failed("wrong number of arguments".to_owned())),
            Err(BindError::Syntax) => Err(CommandError::Failed("syntax error".to_owned())),
            Err(BindError::UnsupportedOptions) => Err(CommandError::Failed("syntax error".to_owned())),
            Err(BindError::InvalidInteger) => Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
            Err(BindError::InvalidExpire) => Err(CommandError::Failed("invalid expire time in 'set' command".to_owned())),
            Err(BindError::EmptyCommand) => Err(CommandError::Failed("empty command".to_owned())),
            Err(BindError::UnsupportedCommand) => Err(unknown_command_error(&request.arguments)),
        };
    }
    // 字符串命令但没有键值执行器：与其余分支一致回 unsupported command。
    Err(CommandError::Failed("unsupported command".to_owned()))
}

/// 用一帧参数执行字符串与键族命令，未知命令按真机文案回复。
///
/// 这是给持有整帧的嵌入宿主用的族入口：监听器按帧分派时用 [`handle_string`] 的
/// `None` 返回值继续找下一个族，宿主只有一条命令要执行时用这里。命令语义、
/// 参数校验与过期判定都在本模块，回复是正式的 `Reply` 值，字节编码归 protocol。
pub fn execute(
    arguments: &[&[u8]],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let request = Request { arguments: arguments.to_vec(), consumed: 0 };
    if let Some(reply) = handle_string(&request, Some(executor), Some(executor), None, namespace, protocol)? {
        return Ok(reply);
    }
    Err(unknown_command_error(arguments))
}

/// 用一帧参数执行键空间族（`KEYS` / `SCAN` / `TYPE` / `OBJECT`），未知命令按真机文案回复。
pub fn execute_keyspace(
    arguments: &[&[u8]],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let request = Request { arguments: arguments.to_vec(), consumed: 0 };
    if let Some(reply) = handle_keyspace(&request, Some(executor), Some(executor), None, namespace, protocol)? {
        return Ok(reply);
    }
    Err(unknown_command_error(arguments))
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
