//! Redis 的 hash / list / set / zset 值类型，落在既有扁平键值合同之上。
//!
//! `.yykv` v3 的字节布局不能改动，`Record` 也没有类型字段。这里把类型编码在
//! **命名空间** 上：字符串继续使用逻辑库命名空间（`redis:<db>`），hash 落在
//! `redis:<db>:hash`，list 落在 `redis:<db>:list`，set 落在 `redis:<db>:set`，
//! zset 落在 `redis:<db>:zset`。命名空间本来就是 `Record.key` 的一部分，所以不新增
//! 字段、不升格式版本，重启后随快照恢复。
//!
//! 这样不会把既有字符串键误判成集合：两类数据位于互不相交的命名空间，
//! 字符串内容再特殊也不会被当成 hash/list；判定类型只是逐个命名空间探测可见记录，
//! 同一逻辑键在任一时刻至多存在一种表示（写入时强制互斥，删除时清干净）。

use std::time::Duration;

use database_storage::cluster_kv::redis::format::{
    decode_hash, decode_list, decode_set, decode_zset, encode_hash, encode_list, encode_set, encode_zset, read_u32, set_blob_count,
    zset_blob_view,
};
use database_types::cluster::Namespace;

use super::{
    KeyValueExecutor,
    blocking::{BlockingRegistry, WaitOutcome, Waiter, timeout_deadline},
    reply::glob_match,
};
use database_protocol_redis::{
    codec::{CommandError, Reply},
    resp::Request,
};
use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};

/// 一个逻辑键当前承载的值类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// 字符串，沿用原命名空间，行为与升级前完全一致。
    String,
    /// 哈希，字段按插入顺序保存在一个值 blob 内。
    Hash,
    /// 列表，元素按从表头到表尾的顺序保存在一个值 blob 内。
    List,
    /// 集合，成员按插入顺序保存在一个值 blob 内（真机小集合同样保持插入顺序）。
    Set,
    /// 有序集合，成员与 `f64` 分数保存在一个值 blob 内，读取时按分数再按成员排序。
    ZSet,
    /// 流，由下游流实现写入同一份键值存储；这里只用于类型探测、删除与枚举。
    Stream,
}

impl KeyKind {
    /// 所有已知类型，按探测顺序排列（字符串优先，保持既有语义）。
    pub const ALL: [KeyKind; 6] = [KeyKind::String, KeyKind::Hash, KeyKind::List, KeyKind::Set, KeyKind::ZSet, KeyKind::Stream];

    /// `TYPE` 回复里使用的类型名。
    pub fn name(self) -> &'static str {
        match self {
            KeyKind::String => "string",
            KeyKind::Hash => "hash",
            KeyKind::List => "list",
            KeyKind::Set => "set",
            KeyKind::ZSet => "zset",
            KeyKind::Stream => "stream",
        }
    }

    /// `SCAN TYPE` 选项里的类型名，未知名称返回 `None`。
    ///
    /// 真机对未知类型名不回错误，只是没有任何键能命中，调用方据此过滤掉全部键。
    pub fn from_name(name: &[u8]) -> Option<KeyKind> {
        KeyKind::ALL.into_iter().find(|kind| kind.name().as_bytes().eq_ignore_ascii_case(name))
    }

    /// 该类型使用的存储命名空间。
    ///
    /// 字符串沿用调用方传入的逻辑库命名空间，其它类型加类型后缀，因此既有数据无需迁移。
    pub fn namespace(self, base: &Namespace) -> Namespace {
        match self {
            KeyKind::String => base.clone(),
            KeyKind::Hash => Namespace(format!("{}:hash", base.0)),
            KeyKind::List => Namespace(format!("{}:list", base.0)),
            KeyKind::Set => Namespace(format!("{}:set", base.0)),
            KeyKind::ZSet => Namespace(format!("{}:zset", base.0)),
            KeyKind::Stream => Namespace(format!("{}:stream", base.0)),
        }
    }
}

/// 探测一个逻辑键当前属于哪种类型。
///
/// 一次探测最多命中一种表示；`Ok(None)` 表示键不存在。执行错误转成可回复的消息，
/// 由调用方决定如何写成 RESP 错误。
pub fn find_kind(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Option<KeyKind>, String> {
    for kind in KeyKind::ALL {
        let command = KeyValueCommand { namespace: kind.namespace(base), key: key.to_vec(), action: KeyValueAction::Exists };
        match executor.execute(None, command) {
            Ok(KeyValueResult::Exists(true)) => return Ok(Some(kind)),
            Ok(KeyValueResult::Exists(false)) => {}
            Ok(_) => return Err("type probe returned an unexpected result".into()),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(None)
}

/// 集合键的一次读取结果。
enum Loaded<T> {
    /// 键不存在或已过期。
    Missing,
    /// 解码成功的值。
    Found(T),
}

/// 读取一个集合键的原始值 blob。
fn load_value(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let command = KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::Get };
    match executor.execute(None, command) {
        Ok(KeyValueResult::Get(value)) => Ok(value),
        Ok(_) => Err("collection lookup returned an unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 写回集合键，保留既有过期时间（Redis 修改集合内容不会清掉 TTL）。
fn store_value(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8], value: Vec<u8>) -> Result<(), String> {
    let command = KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::PutKeepingExpiry(value) };
    match executor.execute(None, command) {
        Ok(KeyValueResult::Put { .. }) => Ok(()),
        Ok(_) => Err("collection write returned an unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 删除集合键，集合清空时使用（Redis 不允许空的 hash / list 继续存在）。
fn remove_value(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<bool, String> {
    let command = KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::Delete };
    match executor.execute(None, command) {
        Ok(KeyValueResult::Delete { removed }) => Ok(removed),
        Ok(_) => Err("collection delete returned an unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// arity 错误的语义消息，由 protocol 编码为 `-ERR ...`。
fn arity_error(command: &str) -> CommandError {
    CommandError::Failed(format!("wrong number of arguments for '{command}' command"))
}

/// 全量 zset 读取的排序规则，与 `sort_zset` 一致。
fn sort_zset_view(entries: &mut [(&[u8], f64)]) {
    entries.sort_by(|left, right| left.1.total_cmp(&right.1).then_with(|| left.0.cmp(right.0)));
}

/// 借用切片形式的 zset 结果，形状与 `zset_entries_reply` 完全一致。
fn zset_view_reply(entries: &[(&[u8], f64)], with_scores: bool, protocol: u8) -> Reply {
    let nested = with_scores && protocol == 3;
    let items: Vec<Reply> = entries
        .iter()
        .flat_map(|(member, score)| {
            let member = Reply::bulk(member.to_vec());
            if !with_scores {
                vec![member]
            }
            else if nested {
                vec![Reply::array(vec![member, score_reply(*score)])]
            }
            else {
                vec![member, score_reply(*score)]
            }
        })
        .collect();
    Reply::array(items)
}

/// 字段与值交错成扁平数组回复（`HGETALL` 形状）。
fn field_value_reply(fields: &[(Vec<u8>, Vec<u8>)]) -> Reply {
    Reply::Array(fields.iter().flat_map(|(field, value)| [Reply::bulk(field.clone()), Reply::bulk(value.clone())]).collect())
}

/// 直接从 set blob 构造成员回复。
///
/// 先走一遍校验再构造，结构损坏时返回 `None`，由调用方报告损坏。
fn set_members_reply(bytes: &[u8]) -> Option<Reply> {
    let count = set_blob_count(bytes)?;
    let mut members = Vec::with_capacity(count);
    let (_, mut rest) = read_u32(bytes)?;
    for _ in 0..count {
        let (length, tail) = read_u32(rest)?;
        let length = length as usize;
        if tail.len() < length {
            return None;
        }
        members.push(Reply::bulk(tail[..length].to_vec()));
        rest = &tail[length..];
    }
    Some(Reply::set(members))
}

/// hash 命令名集合。
pub fn is_hash_command(command: &[u8]) -> bool {
    for name in [
        b"HSET".as_slice(),
        b"HSETNX",
        b"HGET",
        b"HGETALL",
        b"HDEL",
        b"HLEN",
        b"HINCRBY",
        b"HKEYS",
        b"HVALS",
        b"HEXISTS",
        b"HINCRBYFLOAT",
        b"HRANDFIELD",
        b"HSCAN",
    ] {
        if command.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// list 命令名集合。
pub fn is_list_command(command: &[u8]) -> bool {
    for name in [
        b"LPUSH".as_slice(),
        b"RPUSH",
        b"LRANGE",
        b"LPOP",
        b"RPOP",
        b"LLEN",
        b"LINDEX",
        b"LPOS",
        b"LREM",
        b"LSET",
        b"LTRIM",
        b"LINSERT",
        b"RPOPLPUSH",
        b"LMOVE",
        b"LMPOP",
        b"BLPOP",
        b"BRPOP",
        b"BLMOVE",
        b"BRPOPLPUSH",
        b"BLMPOP",
    ] {
        if command.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// set 命令名集合。
pub fn is_set_command(command: &[u8]) -> bool {
    for name in [
        b"SADD".as_slice(),
        b"SREM",
        b"SMEMBERS",
        b"SCARD",
        b"SISMEMBER",
        b"SINTER",
        b"SUNION",
        b"SDIFF",
        b"SINTERSTORE",
        b"SUNIONSTORE",
        b"SDIFFSTORE",
        b"SMOVE",
        b"SRANDMEMBER",
        b"SSCAN",
        b"SPOP",
        b"SMISMEMBER",
        b"SINTERCARD",
    ] {
        if command.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// zset 命令名集合。
pub fn is_zset_command(command: &[u8]) -> bool {
    for name in [
        b"ZADD".as_slice(),
        b"ZSCORE",
        b"ZREM",
        b"ZCARD",
        b"ZRANGE",
        b"ZRANGEBYSCORE",
        b"ZREVRANGEBYSCORE",
        b"ZRANGEBYLEX",
        b"ZREVRANGEBYLEX",
        b"ZREVRANGE",
        b"ZRANGESTORE",
        b"ZRANK",
        b"ZREVRANK",
        b"ZINCRBY",
        b"ZCOUNT",
        b"ZLEXCOUNT",
        b"ZMSCORE",
        b"ZPOPMIN",
        b"ZPOPMAX",
        b"ZRANDMEMBER",
        b"ZMPOP",
        b"BZPOPMIN",
        b"BZPOPMAX",
        b"BZMPOP",
        b"ZREMRANGEBYRANK",
        b"ZREMRANGEBYSCORE",
        b"ZREMRANGEBYLEX",
        b"ZSCAN",
        b"ZUNIONSTORE",
        b"ZINTERSTORE",
        b"ZUNION",
        b"ZINTER",
        b"ZDIFF",
        b"ZDIFFSTORE",
        b"ZINTERCARD",
        b"SORT",
        b"SORT_RO",
    ] {
        if command.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// HyperLogLog 命令名集合。
pub fn is_hyperloglog_command(command: &[u8]) -> bool {
    for name in [b"PFADD".as_slice(), b"PFCOUNT", b"PFMERGE"] {
        if command.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// 载入 hash 字段并校验类型。
fn load_hash(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Loaded<Vec<(Vec<u8>, Vec<u8>)>>, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(Loaded::Missing),
        Ok(Some(KeyKind::Hash)) => {}
        Ok(Some(_)) => {
            return Err(CommandError::WrongType);
        }
        Err(message) => {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    match load_value(executor, &KeyKind::Hash.namespace(base), key) {
        Ok(Some(bytes)) => match decode_hash(&bytes) {
            Some(fields) => Ok(Loaded::Found(fields)),
            None => Err(CommandError::Failed("stored hash value is corrupt".to_owned())),
        },
        Ok(None) => Ok(Loaded::Missing),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 载入 list 元素并校验类型。
fn load_list(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Loaded<Vec<Vec<u8>>>, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(Loaded::Missing),
        Ok(Some(KeyKind::List)) => {}
        Ok(Some(_)) => {
            return Err(CommandError::WrongType);
        }
        Err(message) => {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    match load_value(executor, &KeyKind::List.namespace(base), key) {
        Ok(Some(bytes)) => match decode_list(&bytes) {
            Some(elements) => Ok(Loaded::Found(elements)),
            None => Err(CommandError::Failed("stored list value is corrupt".to_owned())),
        },
        Ok(None) => Ok(Loaded::Missing),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 载入 set 成员并校验类型。
fn load_set(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Loaded<Vec<Vec<u8>>>, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(Loaded::Missing),
        Ok(Some(KeyKind::Set)) => {}
        Ok(Some(_)) => {
            return Err(CommandError::WrongType);
        }
        Err(message) => {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    match load_value(executor, &KeyKind::Set.namespace(base), key) {
        Ok(Some(bytes)) => match decode_set(&bytes) {
            Some(members) => Ok(Loaded::Found(members)),
            None => Err(CommandError::Failed("stored set value is corrupt".to_owned())),
        },
        Ok(None) => Ok(Loaded::Missing),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 载入 zset 成员并校验类型。
fn load_zset(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Loaded<Vec<(Vec<u8>, f64)>>, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(Loaded::Missing),
        Ok(Some(KeyKind::ZSet)) => {}
        Ok(Some(_)) => {
            return Err(CommandError::WrongType);
        }
        Err(message) => {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    match load_value(executor, &KeyKind::ZSet.namespace(base), key) {
        Ok(Some(bytes)) => match decode_zset(&bytes) {
            Some(entries) => Ok(Loaded::Found(entries)),
            None => Err(CommandError::Failed("stored zset value is corrupt".to_owned())),
        },
        Ok(None) => Ok(Loaded::Missing),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 整数参数校验，与真机 `ERR value is not an integer or out of range` 对齐。
pub fn parse_index(raw: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(raw).ok()?;
    let value = text.parse::<i64>().ok()?;
    if value.to_string() != text { None } else { Some(value) }
}

/// zset 分数解析：接受浮点字面量，拒绝 `NaN`（真机不允许 NaN 分数）。
fn parse_score(raw: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(raw).ok()?;
    let score = text.parse::<f64>().ok()?;
    if score.is_nan() { None } else { Some(score) }
}

/// zset 排序：先按分数升序，同分再按成员字节字典序。
fn sort_zset(entries: &mut [(Vec<u8>, f64)]) {
    entries.sort_by(|left, right| left.1.total_cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
}

/// 分数文本：与真机 `d2string` 逐字节一致的最短表示。
///
/// 整数在 `2^62` 以内直接输出十进制；其余走最短往返数字，再按真机的规则在
/// 定点与指数形式之间选择。`inf` / `-inf` 用单词，`-0` 归为 `0`。
fn score_text(score: f64) -> String {
    if score.is_nan() {
        return "nan".to_string();
    }
    if score.is_infinite() {
        return if score < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    if score == 0.0 {
        return "0".to_string();
    }
    if score.fract() == 0.0 && score.abs() <= 4_611_686_018_427_387_904.0 {
        return (score as i64).to_string();
    }
    let negative = score < 0.0;
    let scientific = format!("{:e}", score.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits = mantissa.chars().filter(|character| *character != '.').collect::<String>();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let count = digits.len() as i32;
    // 真机以 `0.digits * 10^(k + count)` 的形式取舍小数点位置。
    let k = exponent + 1 - count;
    let magnitude = exponent.abs();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k >= 0 && magnitude < count + 7 {
        out.push_str(digits);
        for _ in 0..k {
            out.push('0');
        }
    }
    else if k < 0 && (k > -7 || magnitude < 4) {
        let point = count + k;
        if point <= 0 {
            out.push_str("0.");
            for _ in 0..(-point) {
                out.push('0');
            }
            out.push_str(digits);
        }
        else {
            out.push_str(&digits[..point as usize]);
            out.push('.');
            out.push_str(&digits[point as usize..]);
        }
    }
    else {
        out.push(digits.as_bytes()[0] as char);
        if count > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if exponent >= 0 {
            out.push('+');
        }
        out.push_str(&exponent.to_string());
    }
    out
}

/// 分数回复：`Reply::Double` 在 RESP3 编码为 `,` double，在 RESP2 退化为同文本 bulk。
fn score_reply(score: f64) -> Reply {
    Reply::Double(score_text(score))
}

/// 整数的十进制字节文本，用于嵌进 bulk 的元素（如 `SCAN` 游标）。
fn integer_digits(value: i64) -> Vec<u8> {
    let mut buffer = [0_u8; 21];
    let mut index = buffer.len();
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    loop {
        index -= 1;
        buffer[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        index -= 1;
        buffer[index] = b'-';
    }
    buffer[index..].to_vec()
}

/// `HSET` / `HGET` / `HGETALL` / `HDEL` / `HLEN` 的分派入口。
pub fn handle_hash(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    base: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let command = request.arguments[0];
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    if command.eq_ignore_ascii_case(b"HSET") {
        if request.arguments.len() < 4 || (request.arguments.len() - 2) % 2 != 0 {
            return Err(arity_error("hset"));
        }
        let key = request.arguments[1];
        // 字符串或 list 键一律 WRONGTYPE，集合类型之间也不隐式转换。
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::Hash)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut fields = match load_value(executor, &KeyKind::Hash.namespace(base), key) {
            Ok(Some(bytes)) => match decode_hash(&bytes) {
                Some(fields) => fields,
                None => return Err(CommandError::Failed("stored hash value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let mut added = 0_i64;
        for pair in request.arguments[2..].chunks(2) {
            let field = pair[0].to_vec();
            let value = pair[1].to_vec();
            match fields.iter_mut().find(|(name, _)| *name == field) {
                Some(entry) => entry.1 = value,
                None => {
                    fields.push((field, value));
                    added += 1;
                }
            }
        }
        let Some(bytes) = encode_hash(&fields)
        else {
            return Err(CommandError::Failed("hash exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Hash.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::integer(added));
    }
    if command.eq_ignore_ascii_case(b"HGET") {
        if request.arguments.len() != 3 {
            return Err(arity_error("hget"));
        }
        let key = request.arguments[1];
        return match load_hash(executor, base, key)? {
            Loaded::Missing => Ok(Reply::nil()),
            Loaded::Found(fields) => match fields.iter().find(|(name, _)| name.as_slice() == request.arguments[2]) {
                Some((_, value)) => Ok(Reply::bulk(value.to_vec())),
                None => Ok(Reply::nil()),
            },
        };
    }
    if command.eq_ignore_ascii_case(b"HGETALL") {
        if request.arguments.len() != 2 {
            return Err(arity_error("hgetall"));
        }
        let key = request.arguments[1];
        return match load_hash(executor, base, key)? {
            // 键不存在时真机回空数组，而不是 nil。
            Loaded::Missing => Ok(Reply::array(vec![])),
            Loaded::Found(fields) => Ok(field_value_reply(&fields)),
        };
    }
    if command.eq_ignore_ascii_case(b"HDEL") {
        if request.arguments.len() < 3 {
            return Err(arity_error("hdel"));
        }
        let key = request.arguments[1];
        let fields = match load_hash(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(fields) => fields,
        };
        let mut fields = fields;
        let mut removed = 0_i64;
        let names = &request.arguments[2..];
        fields.retain(|(name, _)| {
            let hit = names.iter().any(|candidate| candidate == name);
            if hit {
                removed += 1;
            }
            !hit
        });
        if fields.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::Hash.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_hash(&fields)
            else {
                return Err(CommandError::Failed("hash exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::Hash.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(removed));
    }
    if command.eq_ignore_ascii_case(b"HINCRBY") {
        if request.arguments.len() != 4 {
            return Err(arity_error("hincrby"));
        }
        let delta = match parse_index(request.arguments[3]) {
            Some(delta) => delta,
            None => return Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
        };
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::Hash)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut fields = match load_value(executor, &KeyKind::Hash.namespace(base), key) {
            Ok(Some(bytes)) => match decode_hash(&bytes) {
                Some(fields) => fields,
                None => return Err(CommandError::Failed("stored hash value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let field = request.arguments[2].to_vec();
        let current = match fields.iter().find(|(name, _)| *name == field) {
            Some((_, value)) => match parse_index(value) {
                Some(current) => current,
                None => return Err(CommandError::Failed("hash value is not an integer".to_owned())),
            },
            None => 0,
        };
        let Some(updated) = current.checked_add(delta)
        else {
            return Err(CommandError::Failed("increment or decrement would overflow".to_owned()));
        };
        let encoded = updated.to_string().into_bytes();
        match fields.iter_mut().find(|(name, _)| *name == field) {
            Some(entry) => entry.1 = encoded,
            None => fields.push((field, encoded)),
        }
        let Some(bytes) = encode_hash(&fields)
        else {
            return Err(CommandError::Failed("hash exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Hash.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::integer(updated));
    }
    if command.eq_ignore_ascii_case(b"HKEYS") || command.eq_ignore_ascii_case(b"HVALS") {
        let by_name = command.eq_ignore_ascii_case(b"HKEYS");
        if request.arguments.len() != 2 {
            let name = if by_name { "hkeys" } else { "hvals" };
            return Err(arity_error(name));
        }
        let key = request.arguments[1];
        let fields = match load_hash(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(fields) => fields,
        };
        let items: Vec<Reply> = fields.iter().map(|(field, value)| Reply::bulk(if by_name { field.clone() } else { value.clone() })).collect();
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"HEXISTS") {
        if request.arguments.len() != 3 {
            return Err(arity_error("hexists"));
        }
        let key = request.arguments[1];
        return match load_hash(executor, base, key)? {
            Loaded::Missing => Ok(Reply::integer(0)),
            Loaded::Found(fields) => Ok(Reply::integer(i64::from(fields.iter().any(|(name, _)| name.as_slice() == request.arguments[2])))),
        };
    }
    if command.eq_ignore_ascii_case(b"HSETNX") {
        if request.arguments.len() != 4 {
            return Err(arity_error("hsetnx"));
        }
        let key = request.arguments[1];
        let field = request.arguments[2].to_vec();
        let value = request.arguments[3].to_vec();
        let mut fields = match load_hash(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(fields) => fields,
        };
        if fields.iter().any(|(name, _)| *name == field) {
            return Ok(Reply::integer(0));
        }
        fields.push((field, value));
        let Some(bytes) = encode_hash(&fields)
        else {
            return Err(CommandError::Failed("hash exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Hash.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::integer(1));
    }
    if command.eq_ignore_ascii_case(b"HINCRBYFLOAT") {
        if request.arguments.len() != 4 {
            return Err(arity_error("hincrbyfloat"));
        }
        let Some(increment) = parse_float_literal(std::str::from_utf8(request.arguments[3]).unwrap_or(""))
        else {
            return Err(CommandError::Failed("value is not a valid float".to_owned()));
        };
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::Hash)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut fields = match load_value(executor, &KeyKind::Hash.namespace(base), key) {
            Ok(Some(bytes)) => match decode_hash(&bytes) {
                Some(fields) => fields,
                None => return Err(CommandError::Failed("stored hash value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let field = request.arguments[2].to_vec();
        let current = match fields.iter().find(|(name, _)| *name == field) {
            Some((_, value)) => match parse_float_literal(std::str::from_utf8(value).unwrap_or("")) {
                Some(current) => current,
                None => return Err(CommandError::Failed("hash value is not a float".to_owned())),
            },
            None => 0.0,
        };
        let updated = current + increment;
        if !updated.is_finite() {
            return Err(CommandError::Failed("increment would produce NaN or Infinity".to_owned()));
        }
        let encoded = updated.to_string().into_bytes();
        match fields.iter_mut().find(|(name, _)| *name == field) {
            Some(entry) => entry.1 = encoded.clone(),
            None => fields.push((field, encoded.clone())),
        }
        let Some(bytes) = encode_hash(&fields)
        else {
            return Err(CommandError::Failed("hash exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Hash.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::bulk(encoded.to_vec()));
    }
    if command.eq_ignore_ascii_case(b"HRANDFIELD") {
        if request.arguments.len() < 2 || request.arguments.len() > 4 {
            return Err(arity_error("hrandfield"));
        }
        let key = request.arguments[1];
        let fields = match load_hash(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(fields) => fields,
        };
        let Some(raw_count) = request.arguments.get(2)
        else {
            if fields.is_empty() {
                return Ok(Reply::nil());
            }
            let mut state = random_state();
            let index = (next_random(&mut state) as usize) % fields.len();
            return Ok(Reply::bulk(fields[index].0.to_vec()));
        };
        let Some(count) = parse_index(raw_count)
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        let with_values = match request.arguments.get(3) {
            None => false,
            Some(raw) if raw.eq_ignore_ascii_case(b"WITHVALUES") => true,
            _ => return Err(CommandError::Failed("syntax error".to_owned())),
        };
        if count > 0 {
            let take = (count as usize).min(fields.len());
            let mut order: Vec<usize> = (0..fields.len()).collect();
            let mut state = random_state();
            // Fisher-Yates 洗牌后取前 take 个，保证不重复，与 SRANDMEMBER 同一抽取方式。
            for index in (1..order.len()).rev() {
                let swap = (next_random(&mut state) as usize) % (index + 1);
                order.swap(index, swap);
            }
            let mut items = Vec::with_capacity(if with_values { take * 2 } else { take });
            for index in order.into_iter().take(take) {
                items.push(Reply::bulk(fields[index].0.clone()));
                if with_values {
                    items.push(Reply::bulk(fields[index].1.clone()));
                }
            }
            return Ok(Reply::array(items));
        }
        if count == 0 || fields.is_empty() {
            return Ok(Reply::array(vec![]));
        }
        let picks = count.unsigned_abs() as usize;
        let mut items = Vec::with_capacity(if with_values { picks * 2 } else { picks });
        let mut state = random_state();
        for _ in 0..picks {
            let index = (next_random(&mut state) as usize) % fields.len();
            items.push(Reply::bulk(fields[index].0.clone()));
            if with_values {
                items.push(Reply::bulk(fields[index].1.clone()));
            }
        }
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"HSCAN") {
        if request.arguments.len() < 3 {
            return Err(arity_error("hscan"));
        }
        let cursor = parse_scan_cursor(request.arguments[2])?;
        let (pattern, count) = parse_scan_tail(&request.arguments, 3)?;
        let key = request.arguments[1];
        let fields = match load_hash(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(fields) => fields,
        };
        return Ok(hscan_page_reply(&fields, cursor, count, pattern.as_deref()));
    }
    // HLEN
    if request.arguments.len() != 2 {
        return Err(arity_error("hlen"));
    }
    let key = request.arguments[1];
    match load_hash(executor, base, key)? {
        Loaded::Missing => Ok(Reply::integer(0)),
        Loaded::Found(fields) => Ok(Reply::integer(fields.len() as i64)),
    }
}

/// list 的插入/弹出端，供 `LMOVE` 与 `LINSERT` 解析方向参数。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListSide {
    Left,
    Right,
}

fn parse_side(raw: &[u8]) -> Option<ListSide> {
    if raw.eq_ignore_ascii_case(b"LEFT") {
        Some(ListSide::Left)
    }
    else if raw.eq_ignore_ascii_case(b"RIGHT") {
        Some(ListSide::Right)
    }
    else {
        None
    }
}

/// `LPOS` 选项。
struct LposOptions {
    /// 正数从头部计次，负数从尾部计次，`0` 已在解析阶段拒绝。
    rank: i64,
    /// `Some` 时回复数组，`usize::MAX` 表示 `COUNT 0`（取全部命中）。
    count: Option<usize>,
    /// 最多比较的元素数，`usize::MAX` 表示 `MAXLEN 0`（不限）。
    maxlen: usize,
}

/// 执行 `LPOS`。
fn handle_lpos(executor: &dyn KeyValueExecutor, base: &Namespace, arguments: &[&[u8]], protocol: u8) -> Result<Reply, CommandError> {
    if arguments.len() < 3 {
        return Err(arity_error("lpos"));
    }
    let key = arguments[1];
    let element = arguments[2];
    let mut options = LposOptions { rank: 1, count: None, maxlen: usize::MAX };
    let mut index = 3;
    while index < arguments.len() {
        let option = arguments[index];
        if option.eq_ignore_ascii_case(b"RANK") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            let Some(rank) = parse_index(raw)
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            // 真机把 `RANK` 限制在 `[-i64::MAX, i64::MAX]`，`i64::MIN` 单独报范围错误。
            if rank == i64::MIN {
                return Err(CommandError::Failed(
                    "value is out of range, value must between -9223372036854775807 and 9223372036854775807".to_owned(),
                ));
            }
            if rank == 0 {
                return Err(CommandError::Failed(
                    "RANK can't be zero: use 1 to start from the first match, 2 from the second ... or use negative to start from the end of the list".to_owned(),
                ));
            }
            options.rank = rank;
            index += 2;
            continue;
        }
        if option.eq_ignore_ascii_case(b"COUNT") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            // 真机把解析失败与负数都归为同一条错误，不区分具体原因。
            let Some(count) = parse_index(raw)
            else {
                return Err(CommandError::Failed("COUNT can't be negative".to_owned()));
            };
            if count < 0 {
                return Err(CommandError::Failed("COUNT can't be negative".to_owned()));
            }
            options.count = Some(if count == 0 { usize::MAX } else { count as usize });
            index += 2;
            continue;
        }
        if option.eq_ignore_ascii_case(b"MAXLEN") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            // `MAXLEN` 与 `COUNT` 一致：解析失败与负数共用一条错误。
            let Some(length) = parse_index(raw)
            else {
                return Err(CommandError::Failed("MAXLEN can't be negative".to_owned()));
            };
            if length < 0 {
                return Err(CommandError::Failed("MAXLEN can't be negative".to_owned()));
            }
            options.maxlen = if length == 0 { usize::MAX } else { length as usize };
            index += 2;
            continue;
        }
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let elements = match load_list(executor, base, key)? {
        Loaded::Missing => Vec::new(),
        Loaded::Found(elements) => elements,
    };
    let forward = options.rank > 0;
    let skip = (options.rank.unsigned_abs() - 1) as usize;
    let mut matches: Vec<usize> = Vec::new();
    let mut hits = 0_usize;
    let length = elements.len();
    for step in 0..length.min(options.maxlen) {
        let position = if forward { step } else { length - 1 - step };
        if elements[position].as_slice() == element {
            hits += 1;
            if hits > skip {
                matches.push(position);
                match options.count {
                    None => break,
                    Some(wanted) if matches.len() >= wanted => break,
                    _ => {}
                }
            }
        }
    }
    if options.count.is_some() {
        let items: Vec<Reply> = matches.into_iter().map(|position| Reply::integer(position as i64)).collect();
        return Ok(Reply::array(items));
    }
    match matches.first() {
        Some(position) => Ok(Reply::integer(*position as i64)),
        None => Ok(Reply::nil()),
    }
}

/// 阻塞命令的一次取值尝试。
enum Blocked<T> {
    /// 取到值，可以回复。
    Ready(T),
    /// 键不存在或列表为空，继续等。
    Empty,
}

/// 阻塞取值的最终结果。
enum BlockOutcome<T> {
    /// 从第几个键取到了值。
    Claimed(usize, T),
    /// 截止时间先到，回复 null。
    TimedOut,
}

/// 阻塞取值的公共骨架。
///
/// 先按参数顺序尝试（同一键上只有队首有权取，保证 FIFO），都没取到就睡在
/// 监听器级等待表上，直到某个键被推入或超时。`allow_blocking` 为假时只在
/// `EXEC` 内复放的阻塞命令上出现，按真机的非阻塞语义立刻回一次结果。
fn block_until<T>(
    registry: &BlockingRegistry,
    base: &Namespace,
    keys: &[&[u8]],
    timeout: Option<Duration>,
    allow_blocking: bool,
    mut claim: impl FnMut(usize) -> Result<Blocked<T>, CommandError>,
) -> Result<BlockOutcome<T>, CommandError> {
    if !allow_blocking {
        for index in 0..keys.len() {
            if let Blocked::Ready(value) = claim(index)? {
                return Ok(BlockOutcome::Claimed(index, value));
            }
        }
        return Ok(BlockOutcome::TimedOut);
    }
    let deadline = timeout_deadline(timeout);
    let mut waiters: Vec<Waiter> = keys.iter().map(|key| registry.register(base, key)).collect();
    loop {
        let mut settled: Option<Result<BlockOutcome<T>, CommandError>> = None;
        for (index, waiter) in waiters.iter().enumerate() {
            if !registry.is_head(waiter) {
                continue;
            }
            match claim(index) {
                Ok(Blocked::Ready(value)) => {
                    settled = Some(Ok(BlockOutcome::Claimed(index, value)));
                    break;
                }
                Ok(Blocked::Empty) => {}
                Err(error) => {
                    settled = Some(Err(error));
                    break;
                }
            }
        }
        if let Some(result) = settled {
            registry.unregister_all(&waiters);
            return result;
        }
        match registry.wait_any(&mut waiters, deadline) {
            WaitOutcome::TimedOut => {
                registry.unregister_all(&waiters);
                return Ok(BlockOutcome::TimedOut);
            }
            WaitOutcome::Signaled => {}
        }
    }
}

/// 真机超时上限：秒换算成毫秒后不得超过 `i64::MAX`。
const TIMEOUT_MAX_MILLIS: f64 = i64::MAX as f64;

/// 按真机口径给阻塞超时分类。
///
/// 先按浮点秒解析：非数字与 NaN 回「不是浮点」，无穷或毫秒数越过 `i64::MAX` 回
/// 「越界」；随后向零截断，截断后为负才回「负数」。零与负的亚毫秒值表示无限等待，
/// 正的亚毫秒值按最短 1 毫秒处理，避免把极小正数误当成永久阻塞。`Ok(None)` 表示无限等待。
fn classify_timeout(raw: &[u8]) -> Result<Option<Duration>, &'static str> {
    let text = std::str::from_utf8(raw).map_err(|_| "timeout is not a float or out of range")?;
    let seconds = text.parse::<f64>().map_err(|_| "timeout is not a float or out of range")?;
    if seconds.is_nan() {
        return Err("timeout is not a float or out of range");
    }
    let milliseconds = seconds * 1000.0;
    if seconds.is_infinite() || milliseconds > TIMEOUT_MAX_MILLIS {
        return Err("timeout is out of range");
    }
    let whole = milliseconds.trunc();
    if whole < 0.0 {
        return Err("timeout is negative");
    }
    if whole == 0.0 {
        return Ok(if seconds > 0.0 { Some(Duration::from_millis(1)) } else { None });
    }
    Ok(Some(Duration::from_millis(whole as u64)))
}

/// 解析阻塞命令的超时参数。`0` 表示无限等待。
///
/// 真机把超时按浮点秒解析，负数、非数字与越界分别回不同的错误文案。
fn parse_timeout(raw: &[u8]) -> Result<Option<Duration>, CommandError> {
    classify_timeout(raw).map_err(|message| CommandError::Failed(message.to_owned()))
}

/// 阻塞命令开始前的类型扫描。
///
/// 与真机一致：按参数顺序逐键检查，遇到第一个非空列表就停止，因此它前面的键必须先
/// 通过类型检查；`Err` 表示要回复的错误。
fn precheck_lists(executor: &dyn KeyValueExecutor, base: &Namespace, keys: &[&[u8]]) -> Result<(), CommandError> {
    for key in keys {
        match find_kind(executor, base, key) {
            Ok(None) => {}
            Ok(Some(KeyKind::List)) => match load_value(executor, &KeyKind::List.namespace(base), key) {
                Ok(Some(bytes)) => match decode_list(&bytes) {
                    Some(elements) if elements.is_empty() => {}
                    Some(_) => return Ok(()),
                    None => {
                        return Err(CommandError::Failed("stored list value is corrupt".to_owned()));
                    }
                },
                Ok(None) => {}
                Err(message) => {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            },
            Ok(Some(_)) => {
                return Err(CommandError::WrongType);
            }
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
    }
    Ok(())
}

/// 把内存里的列表写回存储；列表空时删除键，对应真机不允许空列表存在。
fn persist_list(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8], elements: &[Vec<u8>]) -> Result<(), String> {
    if elements.is_empty() {
        remove_value(executor, &KeyKind::List.namespace(base), key)?;
        return Ok(());
    }
    let bytes = encode_list(elements).ok_or_else(|| "list exceeds the storage limit".to_string())?;
    store_value(executor, &KeyKind::List.namespace(base), key, bytes)
}

/// `BLPOP` / `BRPOP`。
fn handle_blocking_pop(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    arguments: &[&[u8]],
    protocol: u8,
    registry: &BlockingRegistry,
    allow_blocking: bool,
) -> Result<Reply, CommandError> {
    let pop_head = arguments[0].eq_ignore_ascii_case(b"BLPOP");
    let name = if pop_head { "blpop" } else { "brpop" };
    if arguments.len() < 3 {
        return Err(arity_error(name));
    }
    let timeout = parse_timeout(arguments[arguments.len() - 1])?;
    let keys: Vec<&[u8]> = arguments[1..arguments.len() - 1].iter().copied().collect();
    precheck_lists(executor, base, &keys)?;
    let outcome = block_until(registry, base, &keys, timeout, allow_blocking, |index| {
        let key = keys[index];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Blocked::Empty),
            Loaded::Found(elements) => elements,
        };
        if elements.is_empty() {
            return Ok(Blocked::Empty);
        }
        let element = if pop_head { elements.remove(0) } else { elements.pop().unwrap_or_default() };
        if let Err(message) = persist_list(executor, base, key, &elements) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        Ok(Blocked::Ready(element))
    })?;
    match outcome {
        BlockOutcome::Claimed(index, element) => Ok(Reply::array(vec![Reply::bulk(keys[index].to_vec()), Reply::bulk(element)])),
        BlockOutcome::TimedOut => Ok(Reply::nil_array()),
    }
}

/// 把源列表的一个元素搬到目标端；失败时错误已经写出。
#[allow(clippy::too_many_arguments)]
fn claim_move(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    registry: &BlockingRegistry,
    source: &[u8],
    destination: &[u8],
    from: ListSide,
    to: ListSide,
) -> Result<Blocked<Vec<u8>>, CommandError> {
    let mut source_elements = match load_list(executor, base, source)? {
        Loaded::Missing => return Ok(Blocked::Empty),
        Loaded::Found(elements) => elements,
    };
    if source_elements.is_empty() {
        return Ok(Blocked::Empty);
    }
    // 目标类型在真正搬运时才校验，与真机一致：源为空时先阻塞，不会因为目标类型出错。
    let mut destination_elements = if source == destination {
        Vec::new()
    }
    else {
        match find_kind(executor, base, destination) {
            Ok(None) | Ok(Some(KeyKind::List)) => {}
            Ok(Some(_)) => {
                return Err(CommandError::WrongType);
            }
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        match load_value(executor, &KeyKind::List.namespace(base), destination) {
            Ok(Some(bytes)) => match decode_list(&bytes) {
                Some(elements) => elements,
                None => {
                    return Err(CommandError::Failed("stored list value is corrupt".to_owned()));
                }
            },
            Ok(None) => Vec::new(),
            Err(message) => {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
    };
    let element = match from {
        ListSide::Left => source_elements.remove(0),
        ListSide::Right => source_elements.pop().unwrap_or_default(),
    };
    if source == destination {
        match to {
            ListSide::Left => source_elements.insert(0, element.clone()),
            ListSide::Right => source_elements.push(element.clone()),
        }
        if let Err(message) = persist_list(executor, base, source, &source_elements) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Blocked::Ready(element));
    }
    match to {
        ListSide::Left => destination_elements.insert(0, element.clone()),
        ListSide::Right => destination_elements.push(element.clone()),
    }
    if let Err(message) = persist_list(executor, base, source, &source_elements) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    if let Err(message) = persist_list(executor, base, destination, &destination_elements) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    // 目标列表多了一个元素，唤醒排在它上面的阻塞消费者。
    registry.notify(base, destination);
    Ok(Blocked::Ready(element))
}

/// `BLMOVE` / `BRPOPLPUSH`。
fn handle_blocking_move(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    arguments: &[&[u8]],
    protocol: u8,
    registry: &BlockingRegistry,
    allow_blocking: bool,
) -> Result<Reply, CommandError> {
    let lmove = arguments[0].eq_ignore_ascii_case(b"BLMOVE");
    let (from, to, timeout_raw) = if lmove {
        if arguments.len() != 6 {
            return Err(arity_error("blmove"));
        }
        let (Some(from), Some(to)) = (parse_side(arguments[3]), parse_side(arguments[4]))
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        };
        (from, to, arguments[5])
    }
    else {
        if arguments.len() != 4 {
            return Err(arity_error("brpoplpush"));
        }
        (ListSide::Right, ListSide::Left, arguments[3])
    };
    let timeout = parse_timeout(timeout_raw)?;
    let source = arguments[1];
    let destination = arguments[2];
    match find_kind(executor, base, source) {
        Ok(None) | Ok(Some(KeyKind::List)) => {}
        Ok(Some(_)) => return Err(CommandError::WrongType),
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    }
    let keys = [source];
    let outcome =
        block_until(registry, base, &keys, timeout, allow_blocking, |_| claim_move(executor, base, registry, source, destination, from, to))?;
    match outcome {
        BlockOutcome::Claimed(_, element) => Ok(Reply::bulk(element)),
        BlockOutcome::TimedOut => Ok(Reply::nil_array()),
    }
}

/// `BLMPOP timeout numkeys key [key ...] LEFT|RIGHT [COUNT count]`。
fn handle_blocking_mpop(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    arguments: &[&[u8]],
    protocol: u8,
    registry: &BlockingRegistry,
    allow_blocking: bool,
) -> Result<Reply, CommandError> {
    if arguments.len() < 5 {
        return Err(arity_error("blmpop"));
    }
    let timeout = parse_timeout(arguments[1])?;
    let Some(numkeys) = parse_index(arguments[2])
    else {
        return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
    };
    if numkeys <= 0 {
        return Err(CommandError::Failed("numkeys should be greater than 0".to_owned()));
    }
    let numkeys = numkeys as usize;
    let keys_end = 3 + numkeys;
    if arguments.len() < keys_end + 1 {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let keys: Vec<&[u8]> = arguments[3..keys_end].to_vec();
    let Some(side) = parse_side(arguments[keys_end])
    else {
        return Err(CommandError::Failed("syntax error".to_owned()));
    };
    let mut count = 1_usize;
    if arguments.len() > keys_end + 1 {
        if arguments.len() != keys_end + 3 || !arguments[keys_end + 1].eq_ignore_ascii_case(b"COUNT") {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let Some(value) = parse_index(arguments[keys_end + 2])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        if value <= 0 {
            return Err(CommandError::Failed("count should be greater than 0".to_owned()));
        }
        count = value as usize;
    }
    precheck_lists(executor, base, &keys)?;
    let outcome = block_until(registry, base, &keys, timeout, allow_blocking, |index| {
        let key = keys[index];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Blocked::Empty),
            Loaded::Found(elements) => elements,
        };
        if elements.is_empty() {
            return Ok(Blocked::Empty);
        }
        let take = count.min(elements.len());
        let popped: Vec<Vec<u8>> = match side {
            ListSide::Left => elements.drain(..take).collect(),
            // 与 `RPOP key count` 一致：从表尾逐个弹出，回复是尾端在前。
            ListSide::Right => elements.drain(elements.len() - take..).rev().collect(),
        };
        if let Err(message) = persist_list(executor, base, key, &elements) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        Ok(Blocked::Ready(popped))
    })?;
    match outcome {
        BlockOutcome::Claimed(index, popped) => {
            let elements: Vec<Reply> = popped.into_iter().map(Reply::bulk).collect();
            Ok(Reply::array(vec![Reply::bulk(keys[index].to_vec()), Reply::array(elements)]))
        }
        BlockOutcome::TimedOut => Ok(Reply::nil_array()),
    }
}

/// `LMPOP numkeys key [key ...] LEFT|RIGHT [COUNT count]`。
///
/// 按参数顺序扫描，第一个非空列表胜出；前面的键先过类型检查，与真机一致。
fn handle_mpop(executor: &dyn KeyValueExecutor, base: &Namespace, arguments: &[&[u8]], protocol: u8) -> Result<Reply, CommandError> {
    if arguments.len() < 4 {
        return Err(arity_error("lmpop"));
    }
    let Some(numkeys) = parse_index(arguments[1])
    else {
        return Err(CommandError::Failed("numkeys should be greater than 0".to_owned()));
    };
    if numkeys <= 0 {
        return Err(CommandError::Failed("numkeys should be greater than 0".to_owned()));
    }
    let numkeys = numkeys as usize;
    let keys_end = 2 + numkeys;
    if arguments.len() < keys_end + 1 {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let Some(side) = parse_side(arguments[keys_end])
    else {
        return Err(CommandError::Failed("syntax error".to_owned()));
    };
    let mut count = 1_usize;
    if arguments.len() > keys_end + 1 {
        if arguments.len() != keys_end + 3 || !arguments[keys_end + 1].eq_ignore_ascii_case(b"COUNT") {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let Some(value) = parse_index(arguments[keys_end + 2])
        else {
            return Err(CommandError::Failed("count should be greater than 0".to_owned()));
        };
        if value <= 0 {
            return Err(CommandError::Failed("count should be greater than 0".to_owned()));
        }
        count = value as usize;
    }
    for key in &arguments[2..keys_end] {
        let elements = match load_list(executor, base, key)? {
            Loaded::Missing => continue,
            Loaded::Found(elements) => elements,
        };
        if elements.is_empty() {
            continue;
        }
        let take = count.min(elements.len());
        let (popped, kept): (Vec<Vec<u8>>, Vec<Vec<u8>>) = match side {
            ListSide::Left => (elements[..take].to_vec(), elements[take..].to_vec()),
            // 与 `RPOP key count` 一致：从表尾逐个弹出，回复是尾端在前。
            ListSide::Right => (elements[elements.len() - take..].iter().rev().cloned().collect(), elements[..elements.len() - take].to_vec()),
        };
        if let Err(message) = persist_list(executor, base, key, &kept) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        let elements: Vec<Reply> = popped.into_iter().map(Reply::bulk).collect();
        return Ok(Reply::array(vec![Reply::bulk(key.to_vec()), Reply::array(elements)]));
    }
    Ok(Reply::nil_array())
}

/// 列表命令的分派入口。
pub fn handle_list(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    base: &Namespace,
    protocol: u8,
    registry: &BlockingRegistry,
    allow_blocking: bool,
) -> Result<Reply, CommandError> {
    let command = request.arguments[0];
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    if command.eq_ignore_ascii_case(b"BLPOP") || command.eq_ignore_ascii_case(b"BRPOP") {
        return handle_blocking_pop(executor, base, &request.arguments, protocol, registry, allow_blocking);
    }
    if command.eq_ignore_ascii_case(b"BLMOVE") || command.eq_ignore_ascii_case(b"BRPOPLPUSH") {
        return handle_blocking_move(executor, base, &request.arguments, protocol, registry, allow_blocking);
    }
    if command.eq_ignore_ascii_case(b"BLMPOP") {
        return handle_blocking_mpop(executor, base, &request.arguments, protocol, registry, allow_blocking);
    }
    if command.eq_ignore_ascii_case(b"LPOS") {
        return handle_lpos(executor, base, &request.arguments, protocol);
    }
    if command.eq_ignore_ascii_case(b"LMPOP") {
        return handle_mpop(executor, base, &request.arguments, protocol);
    }
    if command.eq_ignore_ascii_case(b"LPUSH") || command.eq_ignore_ascii_case(b"RPUSH") {
        let insert_at_head = command.eq_ignore_ascii_case(b"LPUSH");
        let name = if insert_at_head { "lpush" } else { "rpush" };
        if request.arguments.len() < 3 {
            return Err(arity_error(name));
        }
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::List)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut elements = match load_value(executor, &KeyKind::List.namespace(base), key) {
            Ok(Some(bytes)) => match decode_list(&bytes) {
                Some(elements) => elements,
                None => return Err(CommandError::Failed("stored list value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        for value in &request.arguments[2..] {
            if insert_at_head {
                elements.insert(0, value.to_vec());
            }
            else {
                elements.push(value.to_vec());
            }
        }
        let Some(bytes) = encode_list(&elements)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        // 新元素到达，唤醒排在该键上的阻塞消费者。
        registry.notify(base, key);
        return Ok(Reply::integer(elements.len() as i64));
    }
    if command.eq_ignore_ascii_case(b"LINDEX") {
        if request.arguments.len() != 3 {
            return Err(arity_error("lindex"));
        }
        let index = match parse_index(request.arguments[2]) {
            Some(index) => index,
            None => return Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
        };
        let key = request.arguments[1];
        return match load_list(executor, base, key)? {
            Loaded::Missing => Ok(Reply::nil()),
            Loaded::Found(elements) => {
                let length = elements.len() as i64;
                let position = if index < 0 { length.saturating_add(index) } else { index };
                if position < 0 || position >= length { Ok(Reply::nil()) } else { Ok(Reply::bulk(elements[position as usize].clone())) }
            }
        };
    }
    if command.eq_ignore_ascii_case(b"LRANGE") {
        if request.arguments.len() != 4 {
            return Err(arity_error("lrange"));
        }
        let (Some(start), Some(stop)) = (parse_index(request.arguments[2]), parse_index(request.arguments[3]))
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        let key = request.arguments[1];
        let elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::array(vec![])),
            Loaded::Found(elements) => elements,
        };
        let length = elements.len() as i64;
        let mut begin = if start < 0 { length.saturating_add(start) } else { start };
        let mut end = if stop < 0 { length.saturating_add(stop) } else { stop };
        begin = begin.max(0);
        end = end.min(length - 1);
        if begin > end || begin >= length || end < 0 {
            return Ok(Reply::array(vec![]));
        }
        let slice = &elements[begin as usize..=end as usize];
        let items: Vec<Reply> = slice.iter().map(|element| Reply::bulk(element.clone())).collect();
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"LPOP") || command.eq_ignore_ascii_case(b"RPOP") {
        let pop_head = command.eq_ignore_ascii_case(b"LPOP");
        let name = if pop_head { "lpop" } else { "rpop" };
        if request.arguments.len() < 2 || request.arguments.len() > 3 {
            return Err(arity_error(name));
        }
        let with_count = request.arguments.len() == 3;
        let count = if with_count {
            match parse_index(request.arguments[2]) {
                Some(count) if count > 0 => count as usize,
                Some(_) => return Err(CommandError::Failed("value is out of range, must be positive".to_owned())),
                None => return Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
            }
        }
        else {
            1
        };
        let key = request.arguments[1];
        let elements = match load_list(executor, base, key)? {
            Loaded::Missing => {
                return if with_count { Ok(Reply::nil_array()) } else { Ok(Reply::nil()) };
            }
            Loaded::Found(elements) => elements,
        };
        let take = count.min(elements.len());
        let popped: Vec<Vec<u8>> = if pop_head {
            elements[..take].to_vec()
        }
        else {
            // 真机 `RPOP key count` 从表尾开始逐个弹出，回复顺序是尾端在前
            // （`[a,b,c,d]` 取 2 回 `[d,c]`），因此这里要再倒序一次。
            elements[elements.len() - take..].iter().rev().cloned().collect()
        };
        let mut remaining = elements;
        if pop_head {
            remaining.drain(..take);
        }
        else {
            remaining.truncate(remaining.len() - take);
        }
        if remaining.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::List.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_list(&remaining)
            else {
                return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        if with_count {
            let items: Vec<Reply> = popped.iter().map(|element| Reply::bulk(element.clone())).collect();
            return Ok(Reply::array(items));
        }
        // 单值弹出：真机保持 bulk 回复，`count` 缺省时不会回数组。
        return match popped.first() {
            Some(element) => Ok(Reply::bulk(element.to_vec())),
            None => Ok(Reply::nil()),
        };
    }
    if command.eq_ignore_ascii_case(b"LREM") {
        if request.arguments.len() != 4 {
            return Err(arity_error("lrem"));
        }
        let Some(count) = parse_index(request.arguments[2])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        let key = request.arguments[1];
        let element = request.arguments[3];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(elements) => elements,
        };
        let mut removed = 0_i64;
        if count >= 0 {
            let limit = if count == 0 { usize::MAX } else { count as usize };
            let mut kept = Vec::with_capacity(elements.len());
            for existing in elements {
                if (removed as usize) < limit && existing.as_slice() == element {
                    removed += 1;
                }
                else {
                    kept.push(existing);
                }
            }
            elements = kept;
        }
        else {
            let limit = count.unsigned_abs() as usize;
            let mut kept = Vec::with_capacity(elements.len());
            for existing in elements.into_iter().rev() {
                if (removed as usize) < limit && existing.as_slice() == element {
                    removed += 1;
                }
                else {
                    kept.push(existing);
                }
            }
            kept.reverse();
            elements = kept;
        }
        if removed == 0 {
            return Ok(Reply::integer(0));
        }
        if elements.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::List.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_list(&elements)
            else {
                return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(removed));
    }
    if command.eq_ignore_ascii_case(b"LSET") {
        if request.arguments.len() != 4 {
            return Err(arity_error("lset"));
        }
        let Some(index) = parse_index(request.arguments[2])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        let key = request.arguments[1];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Err(CommandError::Failed("no such key".to_owned())),
            Loaded::Found(elements) => elements,
        };
        let length = elements.len() as i64;
        let position = if index < 0 { length.saturating_add(index) } else { index };
        if position < 0 || position >= length {
            return Err(CommandError::Failed("index out of range".to_owned()));
        }
        elements[position as usize] = request.arguments[3].to_vec();
        let Some(bytes) = encode_list(&elements)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::status("OK"));
    }
    if command.eq_ignore_ascii_case(b"LTRIM") {
        if request.arguments.len() != 4 {
            return Err(arity_error("ltrim"));
        }
        let (Some(start), Some(stop)) = (parse_index(request.arguments[2]), parse_index(request.arguments[3]))
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        let key = request.arguments[1];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::status("OK")),
            Loaded::Found(elements) => elements,
        };
        let length = elements.len() as i64;
        let begin = if start < 0 { length.saturating_add(start) } else { start }.max(0);
        let end = if stop < 0 { length.saturating_add(stop) } else { stop }.min(length - 1);
        if begin > end || begin >= length || end < 0 {
            if let Err(message) = remove_value(executor, &KeyKind::List.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
            return Ok(Reply::status("OK"));
        }
        elements.drain(..begin as usize);
        elements.truncate((end - begin + 1) as usize);
        let Some(bytes) = encode_list(&elements)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::status("OK"));
    }
    if command.eq_ignore_ascii_case(b"LINSERT") {
        if request.arguments.len() != 5 {
            return Err(arity_error("linsert"));
        }
        let before = if request.arguments[2].eq_ignore_ascii_case(b"BEFORE") {
            true
        }
        else if request.arguments[2].eq_ignore_ascii_case(b"AFTER") {
            false
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        };
        let key = request.arguments[1];
        let mut elements = match load_list(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(elements) => elements,
        };
        let Some(position) = elements.iter().position(|existing| existing.as_slice() == request.arguments[3])
        else {
            return Ok(Reply::integer(-1));
        };
        let insert_at = if before { position } else { position + 1 };
        elements.insert(insert_at, request.arguments[4].to_vec());
        let Some(bytes) = encode_list(&elements)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        // 新元素到达，唤醒排在该键上的阻塞消费者。
        registry.notify(base, key);
        return Ok(Reply::integer(elements.len() as i64));
    }
    if command.eq_ignore_ascii_case(b"RPOPLPUSH") || command.eq_ignore_ascii_case(b"LMOVE") {
        let lmove = command.eq_ignore_ascii_case(b"LMOVE");
        let (from, to) = if lmove {
            if request.arguments.len() != 5 {
                return Err(arity_error("lmove"));
            }
            let (Some(from), Some(to)) = (parse_side(request.arguments[3]), parse_side(request.arguments[4]))
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            (from, to)
        }
        else {
            if request.arguments.len() != 3 {
                return Err(arity_error("rpoplpush"));
            }
            (ListSide::Right, ListSide::Left)
        };
        let source = request.arguments[1];
        let destination = request.arguments[2];
        let mut elements = match load_list(executor, base, source)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(elements) => elements,
        };
        // 真机先校验源类型，再校验目标类型，最后才判断源键是否为空。
        if source != destination {
            match find_kind(executor, base, destination) {
                Ok(None) | Ok(Some(KeyKind::List)) => {}
                Ok(Some(_)) => return Err(CommandError::WrongType),
                Err(message) => return Err(CommandError::Failed(message.to_owned())),
            }
        }
        let element = if elements.is_empty() {
            None
        }
        else {
            match from {
                ListSide::Left => Some(elements.remove(0)),
                ListSide::Right => elements.pop(),
            }
        };
        let Some(element) = element
        else {
            return Ok(Reply::nil());
        };
        if source == destination {
            match to {
                ListSide::Left => elements.insert(0, element.clone()),
                ListSide::Right => elements.push(element.clone()),
            }
            let Some(bytes) = encode_list(&elements)
            else {
                return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), source, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
            return Ok(Reply::bulk(element.to_vec()));
        }
        if elements.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::List.namespace(base), source) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_list(&elements)
            else {
                return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), source, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        let mut destination_elements = match load_value(executor, &KeyKind::List.namespace(base), destination) {
            Ok(Some(bytes)) => match decode_list(&bytes) {
                Some(elements) => elements,
                None => return Err(CommandError::Failed("stored list value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        match to {
            ListSide::Left => destination_elements.insert(0, element.clone()),
            ListSide::Right => destination_elements.push(element.clone()),
        }
        let Some(bytes) = encode_list(&destination_elements)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::List.namespace(base), destination, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        // 目标端多了一个元素，唤醒排在该键上的阻塞消费者。
        registry.notify(base, destination);
        return Ok(Reply::bulk(element.to_vec()));
    }
    // LLEN
    if request.arguments.len() != 2 {
        return Err(arity_error("llen"));
    }
    let key = request.arguments[1];
    match load_list(executor, base, key)? {
        Loaded::Missing => Ok(Reply::integer(0)),
        Loaded::Found(elements) => Ok(Reply::integer(elements.len() as i64)),
    }
}

/// 集合代数运算。
#[derive(Clone, Copy)]
enum SetOp {
    Inter,
    Union,
    Diff,
}

/// 删除同一逻辑键的全部类型表示，供 `*STORE` 覆盖目标与 `COPY REPLACE` 使用。
fn remove_all_kinds(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<(), String> {
    for kind in KeyKind::ALL {
        remove_value(executor, &kind.namespace(base), key)?;
    }
    Ok(())
}

/// 无条件写入一个值（新记录不带过期时间），对应 `*STORE` 覆盖目标的真机语义。
fn store_fresh_value(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8], value: Vec<u8>) -> Result<(), String> {
    let command = KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::Put(value) };
    match executor.execute(None, command) {
        Ok(KeyValueResult::Put { .. }) => Ok(()),
        Ok(_) => Err("fresh write returned an unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 读取多个集合并校验类型；任一键类型不合法时整条命令按真机回 `WRONGTYPE`。
fn load_sets(executor: &dyn KeyValueExecutor, base: &Namespace, keys: &[&[u8]]) -> Result<Vec<Vec<Vec<u8>>>, CommandError> {
    let mut sets = Vec::with_capacity(keys.len());
    for key in keys {
        match load_set(executor, base, key)? {
            Loaded::Missing => sets.push(Vec::new()),
            Loaded::Found(members) => sets.push(members),
        }
    }
    Ok(sets)
}

/// 按真机语义计算交集、并集或差集，保留第一个操作数的成员顺序。
fn apply_set_op(op: SetOp, sets: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    match op {
        SetOp::Union => {
            let mut union: Vec<Vec<u8>> = Vec::new();
            for set in sets {
                for member in set {
                    if !union.iter().any(|existing| existing == member) {
                        union.push(member.clone());
                    }
                }
            }
            union
        }
        SetOp::Inter => {
            let mut result = sets.first().cloned().unwrap_or_default();
            result.retain(|member| sets.iter().all(|set| set.iter().any(|candidate| candidate == member)));
            result
        }
        SetOp::Diff => {
            let mut result = sets.first().cloned().unwrap_or_default();
            for set in &sets[1..] {
                result.retain(|member| !set.iter().any(|candidate| candidate == member));
            }
            result
        }
    }
}

/// 进程内伪随机源：没有引入 `rand` 依赖，用时钟纳秒做种子跑 xorshift64。
fn random_state() -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_nanos() as u64).unwrap_or(1);
    nanos ^ 0x9E37_79B9_7F4A_7C15
}

fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

/// 解析集合扫描的游标：只接受规范十进制写法。
fn parse_scan_cursor(raw: &[u8]) -> Result<usize, CommandError> {
    let text = match std::str::from_utf8(raw) {
        Ok(text) if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => text,
        _ => {
            return Err(CommandError::Failed("invalid cursor".to_owned()));
        }
    };
    match text.parse::<usize>() {
        Ok(cursor) => Ok(cursor),
        Err(_) => Err(CommandError::Failed("invalid cursor".to_owned())),
    }
}

/// 解析 `MATCH pattern` 与 `COUNT count` 选项。
fn parse_scan_tail(arguments: &[&[u8]], start: usize) -> Result<(Option<Vec<u8>>, usize), CommandError> {
    let mut pattern: Option<Vec<u8>> = None;
    let mut count = 10usize;
    let mut index = start;
    while index < arguments.len() {
        if index + 1 >= arguments.len() {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        if arguments[index].eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(arguments[index + 1].to_vec());
        }
        else if arguments[index].eq_ignore_ascii_case(b"COUNT") {
            let Ok(value) = std::str::from_utf8(arguments[index + 1]).unwrap_or("").parse::<u64>()
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            if value == 0 {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            count = value.min(usize::MAX as u64) as usize;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        index += 2;
    }
    Ok((pattern, count))
}

/// 一段集合扫描结果：`[next-cursor, [members...]]`。
fn scan_page_reply(items: &[Vec<u8>], cursor: usize, count: usize, pattern: Option<&[u8]>) -> Reply {
    if cursor >= items.len() {
        return Reply::array(vec![Reply::bulk(b"0".to_vec()), Reply::array(vec![])]);
    }
    let end = cursor.saturating_add(count).min(items.len());
    let next = if end >= items.len() { "0".to_string() } else { end.to_string() };
    let matched: Vec<Reply> = items[cursor..end]
        .iter()
        .filter(|item| pattern.map_or(true, |pattern| glob_match(pattern, item)))
        .map(|item| Reply::bulk(item.clone()))
        .collect();
    Reply::array(vec![Reply::bulk(next.into_bytes()), Reply::array(matched)])
}

/// `SADD` / `SREM` / `SMEMBERS` / `SCARD` / `SISMEMBER` 及集合代数、移动、随机读取与扫描的分派入口。
pub fn handle_set(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    base: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let command = request.arguments[0];
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    if command.eq_ignore_ascii_case(b"SADD") {
        if request.arguments.len() < 3 {
            return Err(arity_error("sadd"));
        }
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::Set)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut members = match load_value(executor, &KeyKind::Set.namespace(base), key) {
            Ok(Some(bytes)) => match decode_set(&bytes) {
                Some(members) => members,
                None => return Err(CommandError::Failed("stored set value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let mut added = 0_i64;
        for member in &request.arguments[2..] {
            if !members.iter().any(|existing| existing == member) {
                members.push(member.to_vec());
                added += 1;
            }
        }
        // 全是重复成员时不重写值，已有集合连同 TTL 原样保留。
        if added > 0 {
            let Some(bytes) = encode_set(&members)
            else {
                return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(added));
    }
    if command.eq_ignore_ascii_case(b"SREM") {
        if request.arguments.len() < 3 {
            return Err(arity_error("srem"));
        }
        let key = request.arguments[1];
        let mut members = match load_set(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(members) => members,
        };
        let mut removed = 0_i64;
        members.retain(|existing| {
            let hit = request.arguments[2..].iter().any(|candidate| candidate == existing);
            if hit {
                removed += 1;
            }
            !hit
        });
        // 没有任何成员被移除时集合内容不变：不重写同值记录，修订号也就不推进，
        // `WATCH` 不会因为一次未命中的 `SREM` 误判键被改过。
        if removed == 0 {
            return Ok(Reply::integer(0));
        }
        if members.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::Set.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_set(&members)
            else {
                return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(removed));
    }
    if command.eq_ignore_ascii_case(b"SMEMBERS") {
        if request.arguments.len() != 2 {
            return Err(arity_error("smembers"));
        }
        let key = request.arguments[1];
        // 全量读取不需要逐成员解码：类型探测之后直接从成员 blob 写回复，
        // 这样一个大集合只分配一次值拷贝，而不是每个成员一个 `Vec<u8>`。
        match find_kind(executor, base, key) {
            Ok(None) => return Ok(Reply::set(vec![])),
            Ok(Some(KeyKind::Set)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        return match load_value(executor, &KeyKind::Set.namespace(base), key) {
            Ok(Some(bytes)) => match set_members_reply(&bytes) {
                Some(reply) => Ok(reply),
                None => Err(CommandError::Failed("stored set value is corrupt".to_owned())),
            },
            Ok(None) => Ok(Reply::set(vec![])),
            Err(message) => Err(CommandError::Failed(message.to_owned())),
        };
    }
    if command.eq_ignore_ascii_case(b"SCARD") {
        if request.arguments.len() != 2 {
            return Err(arity_error("scard"));
        }
        let key = request.arguments[1];
        return match load_set(executor, base, key)? {
            Loaded::Missing => Ok(Reply::integer(0)),
            Loaded::Found(members) => Ok(Reply::integer(members.len() as i64)),
        };
    }
    if command.eq_ignore_ascii_case(b"SINTER") || command.eq_ignore_ascii_case(b"SUNION") || command.eq_ignore_ascii_case(b"SDIFF") {
        if request.arguments.len() < 2 {
            let name = String::from_utf8_lossy(command).to_ascii_lowercase();
            return Err(arity_error(&name));
        }
        let op = if command.eq_ignore_ascii_case(b"SINTER") {
            SetOp::Inter
        }
        else if command.eq_ignore_ascii_case(b"SUNION") {
            SetOp::Union
        }
        else {
            SetOp::Diff
        };
        let keys: Vec<&[u8]> = request.arguments[1..].to_vec();
        let sets = load_sets(executor, base, &keys)?;
        let result = apply_set_op(op, &sets);
        let items: Vec<Reply> = result.iter().map(|member| Reply::bulk(member.clone())).collect();
        return Ok(Reply::set(items));
    }
    if command.eq_ignore_ascii_case(b"SINTERSTORE")
        || command.eq_ignore_ascii_case(b"SUNIONSTORE")
        || command.eq_ignore_ascii_case(b"SDIFFSTORE")
    {
        if request.arguments.len() < 3 {
            let name = String::from_utf8_lossy(command).to_ascii_lowercase();
            return Err(arity_error(&name));
        }
        let op = if command.eq_ignore_ascii_case(b"SINTERSTORE") {
            SetOp::Inter
        }
        else if command.eq_ignore_ascii_case(b"SUNIONSTORE") {
            SetOp::Union
        }
        else {
            SetOp::Diff
        };
        let destination = request.arguments[1];
        let keys: Vec<&[u8]> = request.arguments[2..].to_vec();
        let sets = load_sets(executor, base, &keys)?;
        let result = apply_set_op(op, &sets);
        // `*STORE` 覆盖目标且不继承旧过期时间；结果为空时目标键整体删除。
        if let Err(message) = remove_all_kinds(executor, base, destination) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        if !result.is_empty() {
            let Some(bytes) = encode_set(&result)
            else {
                return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_fresh_value(executor, &KeyKind::Set.namespace(base), destination, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(result.len() as i64));
    }
    if command.eq_ignore_ascii_case(b"SMOVE") {
        if request.arguments.len() != 4 {
            return Err(arity_error("smove"));
        }
        let source = request.arguments[1];
        let destination = request.arguments[2];
        let member = request.arguments[3];
        if source == destination {
            return match load_set(executor, base, source)? {
                Loaded::Missing => Ok(Reply::integer(0)),
                Loaded::Found(members) => Ok(Reply::integer(i64::from(members.iter().any(|existing| existing.as_slice() == member)))),
            };
        }
        // 源键不存在时真机直接回 0，不再探测目标类型。
        let mut members = match load_set(executor, base, source)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(members) => members,
        };
        match find_kind(executor, base, destination) {
            Ok(None) | Ok(Some(KeyKind::Set)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let Some(position) = members.iter().position(|existing| existing.as_slice() == member)
        else {
            return Ok(Reply::integer(0));
        };
        members.remove(position);
        if members.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::Set.namespace(base), source) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_set(&members)
            else {
                return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), source, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        let mut destination_members = match load_value(executor, &KeyKind::Set.namespace(base), destination) {
            Ok(Some(bytes)) => match decode_set(&bytes) {
                Some(members) => members,
                None => return Err(CommandError::Failed("stored set value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        if !destination_members.iter().any(|existing| existing.as_slice() == member) {
            destination_members.push(member.to_vec());
        }
        let Some(bytes) = encode_set(&destination_members)
        else {
            return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), destination, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::integer(1));
    }
    if command.eq_ignore_ascii_case(b"SRANDMEMBER") {
        if request.arguments.len() < 2 || request.arguments.len() > 3 {
            return Err(arity_error("srandmember"));
        }
        let key = request.arguments[1];
        let members = match load_set(executor, base, key)? {
            Loaded::Missing => {
                return if request.arguments.len() == 3 { Ok(Reply::array(vec![])) } else { Ok(Reply::nil()) };
            }
            Loaded::Found(members) => members,
        };
        let count = match request.arguments.get(2) {
            None => {
                if members.is_empty() {
                    return Ok(Reply::nil());
                }
                let mut state = random_state();
                let index = (next_random(&mut state) as usize) % members.len();
                return Ok(Reply::bulk(members[index].to_vec()));
            }
            Some(raw) => match parse_index(raw) {
                Some(count) => count,
                None => return Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
            },
        };
        if count >= 0 {
            let take = (count as usize).min(members.len());
            let mut order: Vec<usize> = (0..members.len()).collect();
            let mut state = random_state();
            // Fisher-Yates 洗牌后取前 take 个，保证不重复。
            for index in (1..order.len()).rev() {
                let swap = (next_random(&mut state) as usize) % (index + 1);
                order.swap(index, swap);
            }
            let items: Vec<Reply> = order.into_iter().take(take).map(|index| Reply::bulk(members[index].clone())).collect();
            return Ok(Reply::array(items));
        }
        if members.is_empty() {
            return Ok(Reply::array(vec![]));
        }
        let mut items = Vec::with_capacity(count.unsigned_abs() as usize);
        let mut state = random_state();
        for _ in 0..count.unsigned_abs() {
            let index = (next_random(&mut state) as usize) % members.len();
            items.push(Reply::bulk(members[index].clone()));
        }
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"SSCAN") {
        if request.arguments.len() < 3 {
            return Err(arity_error("sscan"));
        }
        let cursor = parse_scan_cursor(request.arguments[2])?;
        let (pattern, count) = parse_scan_tail(&request.arguments, 3)?;
        let key = request.arguments[1];
        let members = match load_set(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(members) => members,
        };
        return Ok(scan_page_reply(&members, cursor, count, pattern.as_deref()));
    }
    if command.eq_ignore_ascii_case(b"SPOP") {
        if request.arguments.len() < 2 || request.arguments.len() > 3 {
            return Err(arity_error("spop"));
        }
        let count = match request.arguments.get(2) {
            None => None,
            Some(raw) => match parse_index(raw) {
                Some(count) if count >= 0 => Some(count as usize),
                Some(_) => return Err(CommandError::Failed("value is out of range, must be positive".to_owned())),
                None => return Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
            },
        };
        let Some(count) = count
        else {
            let mut members = match load_set(executor, base, request.arguments[1])? {
                Loaded::Missing => return Ok(Reply::nil()),
                Loaded::Found(members) => members,
            };
            if members.is_empty() {
                return Ok(Reply::nil());
            }
            let mut state = random_state();
            let index = (next_random(&mut state) as usize) % members.len();
            let member = members.remove(index);
            if members.is_empty() {
                if let Err(message) = remove_value(executor, &KeyKind::Set.namespace(base), request.arguments[1]) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
            else {
                let Some(bytes) = encode_set(&members)
                else {
                    return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
                };
                if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), request.arguments[1], bytes) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
            return Ok(Reply::bulk(member.to_vec()));
        };
        if count == 0 {
            return Ok(Reply::set(vec![]));
        }
        let members = match load_set(executor, base, request.arguments[1])? {
            Loaded::Missing => return Ok(Reply::set(vec![])),
            Loaded::Found(members) => members,
        };
        if members.is_empty() {
            return Ok(Reply::set(vec![]));
        }
        let take = count.min(members.len());
        let mut order: Vec<usize> = (0..members.len()).collect();
        let mut state = random_state();
        // 部分 Fisher-Yates 洗牌，取前 take 个保证不重复。
        for index in 0..take {
            let swap = index + (next_random(&mut state) as usize) % (members.len() - index);
            order.swap(index, swap);
        }
        let popped: Vec<Reply> = order.iter().take(take).map(|index| Reply::bulk(members[*index].clone())).collect();
        let reply = Reply::set(popped);
        if take == members.len() {
            if let Err(message) = remove_value(executor, &KeyKind::Set.namespace(base), request.arguments[1]) {
                return Err(CommandError::Failed(message.to_owned()));
            }
            return Ok(reply);
        }
        let mut kept = vec![true; members.len()];
        for index in order.iter().take(take) {
            kept[*index] = false;
        }
        let remaining =
            members.iter().enumerate().filter(|(index, _)| kept[*index]).map(|(_, member)| member.clone()).collect::<Vec<Vec<u8>>>();
        let Some(bytes) = encode_set(&remaining)
        else {
            return Err(CommandError::Failed("set exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::Set.namespace(base), request.arguments[1], bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(reply);
    }
    if command.eq_ignore_ascii_case(b"SMISMEMBER") {
        if request.arguments.len() < 3 {
            return Err(arity_error("smismember"));
        }
        let members = match load_set(executor, base, request.arguments[1])? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(members) => members,
        };
        let items: Vec<Reply> = request.arguments[2..]
            .iter()
            .map(|member| Reply::integer(i64::from(members.iter().any(|existing| existing.as_slice() == *member))))
            .collect();
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"SINTERCARD") {
        if request.arguments.len() < 3 {
            return Err(arity_error("sintercard"));
        }
        let numkeys = match parse_index(request.arguments[1]) {
            Some(numkeys) if numkeys > 0 => numkeys as usize,
            _ => return Err(CommandError::Failed("numkeys should be greater than 0".to_owned())),
        };
        if request.arguments.len() < 2 + numkeys {
            return Err(CommandError::Failed("Number of keys can't be greater than number of args".to_owned()));
        }
        let mut limit = 0usize;
        if request.arguments.len() > 2 + numkeys {
            if !request.arguments[2 + numkeys].eq_ignore_ascii_case(b"LIMIT") || request.arguments.len() != 4 + numkeys {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            match parse_index(request.arguments[3 + numkeys]) {
                Some(value) if value >= 0 => limit = value as usize,
                _ => return Err(CommandError::Failed("LIMIT can't be negative".to_owned())),
            }
        }
        let keys = request.arguments[2..2 + numkeys].to_vec();
        let sets = load_sets(executor, base, &keys)?;
        let found = apply_set_op(SetOp::Inter, &sets).len();
        let result = if limit == 0 { found } else { found.min(limit) };
        return Ok(Reply::integer(result as i64));
    }
    // SISMEMBER
    if request.arguments.len() != 3 {
        return Err(arity_error("sismember"));
    }
    let key = request.arguments[1];
    match load_set(executor, base, key)? {
        Loaded::Missing => Ok(Reply::integer(0)),
        Loaded::Found(members) => {
            let present = members.iter().any(|member| member.as_slice() == request.arguments[2]);
            Ok(Reply::integer(i64::from(present)))
        }
    }
}

/// 分数区间端点：`(` 前缀表示开区间，`-inf` / `+inf` 表示无穷。
#[derive(Clone, Copy)]
struct ScoreBound {
    value: f64,
    exclusive: bool,
}

/// 解析浮点字面量，接受 `-inf` / `+inf` / `inf` 并拒绝 `NaN`。
fn parse_float_literal(text: &str) -> Option<f64> {
    let normalized = text.to_ascii_lowercase();
    if normalized == "-inf" {
        return Some(f64::NEG_INFINITY);
    }
    if normalized == "+inf" || normalized == "inf" {
        return Some(f64::INFINITY);
    }
    let value = text.parse::<f64>().ok()?;
    if value.is_nan() { None } else { Some(value) }
}

fn parse_score_bound(raw: &[u8]) -> Option<ScoreBound> {
    let text = std::str::from_utf8(raw).ok()?;
    if let Some(rest) = text.strip_prefix('(') {
        let value = parse_float_literal(rest)?;
        return Some(ScoreBound { value, exclusive: true });
    }
    let value = parse_float_literal(text)?;
    Some(ScoreBound { value, exclusive: false })
}

fn score_at_least(score: f64, bound: ScoreBound) -> bool {
    if bound.exclusive { score > bound.value } else { score >= bound.value }
}

fn score_at_most(score: f64, bound: ScoreBound) -> bool {
    if bound.exclusive { score < bound.value } else { score <= bound.value }
}

fn in_score_range(score: f64, min: ScoreBound, max: ScoreBound) -> bool {
    score_at_least(score, min) && score_at_most(score, max)
}

/// 字典区间端点：`-` / `+` 表示无界，`[x` 闭区间，`(x` 开区间。
#[derive(Clone)]
enum LexBound {
    NegInf,
    PosInf,
    Value { bytes: Vec<u8>, exclusive: bool },
}

fn parse_lex_bound(raw: &[u8]) -> Option<LexBound> {
    match raw {
        b"-" => Some(LexBound::NegInf),
        b"+" => Some(LexBound::PosInf),
        _ => {
            let (exclusive, rest) = match raw.split_first() {
                Some((b'[', rest)) => (false, rest),
                Some((b'(', rest)) => (true, rest),
                _ => return None,
            };
            Some(LexBound::Value { bytes: rest.to_vec(), exclusive })
        }
    }
}

fn lex_at_least(member: &[u8], bound: &LexBound) -> bool {
    match bound {
        LexBound::NegInf => true,
        LexBound::PosInf => false,
        LexBound::Value { bytes, exclusive } => {
            if *exclusive {
                member > bytes.as_slice()
            }
            else {
                member >= bytes.as_slice()
            }
        }
    }
}

fn lex_at_most(member: &[u8], bound: &LexBound) -> bool {
    match bound {
        LexBound::NegInf => false,
        LexBound::PosInf => true,
        LexBound::Value { bytes, exclusive } => {
            if *exclusive {
                member < bytes.as_slice()
            }
            else {
                member <= bytes.as_slice()
            }
        }
    }
}

/// 真机在 (score, member) 顺序上做字典范围查找，再取命中的连续一段。
///
/// 分数一致时这就是普通的字典序；分数不一致时保留这条遍历语义，包括首元素
/// 越过 `max` 时整段判空。
fn lex_block(entries: &[(Vec<u8>, f64)], min: &LexBound, max: &LexBound, reverse: bool) -> Option<(usize, usize)> {
    let (first, last) = entries.first().zip(entries.last())?;
    if !lex_at_most(&first.0, max) || !lex_at_least(&last.0, min) {
        return None;
    }
    if reverse {
        let mut end = entries.len() - 1;
        while end > 0 && !lex_at_most(&entries[end].0, max) {
            end -= 1;
        }
        if !lex_at_most(&entries[end].0, max) || !lex_at_least(&entries[end].0, min) {
            return None;
        }
        let mut start = end;
        while start > 0 && lex_at_least(&entries[start - 1].0, min) {
            start -= 1;
        }
        Some((start, end))
    }
    else {
        let Some(start) = entries.iter().position(|(member, _)| lex_at_least(member, min))
        else {
            return None;
        };
        if !lex_at_most(&entries[start].0, max) {
            return None;
        }
        let mut end = start;
        while end + 1 < entries.len() && lex_at_most(&entries[end + 1].0, max) {
            end += 1;
        }
        Some((start, end))
    }
}

/// 把 `start` / `stop` 归一化为可切片的排名区间；空区间回 `None`。
fn entry_rank_range(start: i64, stop: i64, length: usize) -> Option<(usize, usize)> {
    let length = length as i64;
    let begin = if start < 0 { length.saturating_add(start) } else { start }.max(0);
    let end = if stop < 0 { length.saturating_add(stop) } else { stop }.min(length - 1);
    if begin > end || begin >= length || end < 0 { None } else { Some((begin as usize, end as usize)) }
}

/// 按 `LIMIT` 截取一段结果；offset 为负按 0 处理，count 为负表示取到末尾。
fn apply_limit<T: Clone>(items: &[T], offset: i64, count: i64) -> Vec<T> {
    let offset = offset.max(0) as usize;
    if offset >= items.len() {
        return Vec::new();
    }
    let remaining = &items[offset..];
    if count < 0 {
        return remaining.to_vec();
    }
    remaining[..(count as usize).min(remaining.len())].to_vec()
}

/// 一段 zset 结果，`WITHSCORES` 时成员与分数交替排列。
///
/// RESP2 把分数作为相邻的 bulk 平铺，RESP3 改用 `[member, score]` 子数组，
/// 与真机的 `addReplyArrayLen` / `addReplyDouble` 组合一致。
fn zset_entries_reply(entries: &[(Vec<u8>, f64)], with_scores: bool, protocol: u8) -> Reply {
    let borrowed: Vec<(&[u8], f64)> = entries.iter().map(|(member, score)| (member.as_slice(), *score)).collect();
    zset_view_reply(&borrowed, with_scores, protocol)
}

/// `[member, score]` 子数组序列，`ZPOPMIN` / `ZMPOP` 等使用。
fn zset_pairs_reply(entries: &[(Vec<u8>, f64)], protocol: u8) -> Reply {
    Reply::Array(entries.iter().map(|(member, score)| Reply::array(vec![Reply::bulk(member.clone()), score_reply(*score)])).collect())
}

/// `ZRANGE` / `ZRANGESTORE` 的范围模式。
#[derive(Clone, Copy, PartialEq)]
enum RangeMode {
    Rank,
    Score,
    Lex,
}

/// 一条范围查询解析后的选项。
struct RangeRequest {
    mode: RangeMode,
    reverse: bool,
    with_scores: bool,
    limit: Option<(i64, i64)>,
    first: Vec<u8>,
    second: Vec<u8>,
}

/// 解析 `BYSCORE` / `BYLEX` / `REV` / `LIMIT` / `WITHSCORES` 的组合。
///
/// `bounds` 是两个端点在参数里的下标，`options_start` 是选项起点。互斥与配套
/// 规则按真机：`LIMIT` 只服务范围模式，`BYLEX` 拒绝 `WITHSCORES`。
fn parse_range_request(
    arguments: &[&[u8]],
    bounds: (usize, usize),
    options_start: usize,
    allow_scores: bool,
) -> Result<RangeRequest, CommandError> {
    let mut request = RangeRequest {
        mode: RangeMode::Rank,
        reverse: false,
        with_scores: false,
        limit: None,
        first: arguments[bounds.0].to_vec(),
        second: arguments[bounds.1].to_vec(),
    };
    let mut index = options_start;
    while index < arguments.len() {
        let option = arguments[index];
        if option.eq_ignore_ascii_case(b"BYSCORE") {
            request.mode = RangeMode::Score;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"BYLEX") {
            request.mode = RangeMode::Lex;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"REV") {
            request.reverse = true;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"LIMIT") {
            if index + 2 >= arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            let (Some(offset), Some(count)) = (parse_index(arguments[index + 1]), parse_index(arguments[index + 2]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            request.limit = Some((offset, count));
            index += 3;
        }
        else if option.eq_ignore_ascii_case(b"WITHSCORES") && allow_scores {
            request.with_scores = true;
            index += 1;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    if request.mode == RangeMode::Lex && request.with_scores {
        return Err(CommandError::Failed("syntax error, WITHSCORES not supported in combination with BYLEX".to_owned()));
    }
    if request.limit.is_some() && request.mode == RangeMode::Rank {
        return Err(CommandError::Failed("syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX".to_owned()));
    }
    Ok(request)
}

/// `ZRANGE key 0 -1 [WITHSCORES]` 的整段读形态。
///
/// 只有这两个下标都写成规范形式时才走借用快路径，其它写法继续走通用解析，
/// 这样语法错误、`REV`、`BYSCORE` / `BYLEX` 与 `LIMIT` 的语义完全不变。
fn is_full_zset_range(arguments: &[&[u8]]) -> bool {
    if arguments.len() != 4 && arguments.len() != 5 {
        return false;
    }
    if arguments.len() == 5 && !arguments[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return false;
    }
    arguments[2] == b"0" && arguments[3] == b"-1"
}

/// 整段读取一个 zset，直接从值 blob 输出成员与分数。
///
/// 通用路径会先 `decode_zset` 逐成员分配，再对选中区间做一次 `to_vec`
/// 二次克隆；整段读只需要一份借用视图即可，分配量与成员数无关。
fn read_full_zset(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    key: &[u8],
    with_scores: bool,
    protocol: u8,
) -> Result<Reply, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(Reply::array(vec![])),
        Ok(Some(KeyKind::ZSet)) => {}
        Ok(Some(_)) => return Err(CommandError::WrongType),
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    }
    match load_value(executor, &KeyKind::ZSet.namespace(base), key) {
        Ok(Some(bytes)) => match zset_blob_view(&bytes) {
            Some(mut entries) => {
                sort_zset_view(&mut entries);
                Ok(zset_view_reply(&entries, with_scores, protocol))
            }
            None => Err(CommandError::Failed("stored zset value is corrupt".to_owned())),
        },
        Ok(None) => Ok(Reply::array(vec![])),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 按键取出范围结果。
fn select_range(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    key: &[u8],
    request: &RangeRequest,
) -> Result<Vec<(Vec<u8>, f64)>, CommandError> {
    let mut entries = match load_zset(executor, base, key)? {
        Loaded::Missing => Vec::new(),
        Loaded::Found(entries) => entries,
    };
    let selected = match request.mode {
        RangeMode::Rank => {
            let (Some(start), Some(stop)) = (parse_index(&request.first), parse_index(&request.second))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            sort_zset(&mut entries);
            if request.reverse {
                entries.reverse();
            }
            match entry_rank_range(start, stop, entries.len()) {
                Some((begin, end)) => entries[begin..=end].to_vec(),
                None => Vec::new(),
            }
        }
        RangeMode::Score => {
            let (low, high) = if request.reverse { (&request.second, &request.first) } else { (&request.first, &request.second) };
            let (Some(min), Some(max)) = (parse_score_bound(low), parse_score_bound(high))
            else {
                return Err(CommandError::Failed("min or max is not a float".to_owned()));
            };
            sort_zset(&mut entries);
            let mut selected: Vec<(Vec<u8>, f64)> = entries.into_iter().filter(|(_, score)| in_score_range(*score, min, max)).collect();
            if request.reverse {
                selected.reverse();
            }
            selected
        }
        RangeMode::Lex => {
            let (low, high) = if request.reverse { (&request.second, &request.first) } else { (&request.first, &request.second) };
            let (Some(min), Some(max)) = (parse_lex_bound(low), parse_lex_bound(high))
            else {
                return Err(CommandError::Failed("min or max not valid string range item".to_owned()));
            };
            // 真机按 (score, member) 顺序定位首尾再整段输出，分数不一致时
            // 保留这条遍历语义。
            sort_zset(&mut entries);
            let mut selected: Vec<(Vec<u8>, f64)> = match lex_block(&entries, &min, &max, request.reverse) {
                Some((start, end)) => entries[start..=end].to_vec(),
                None => Vec::new(),
            };
            if request.reverse {
                selected.reverse();
            }
            selected
        }
    };
    match request.limit {
        Some((offset, count)) => Ok(apply_limit(&selected, offset, count)),
        None => Ok(selected),
    }
}

/// `ZADD` 的选项组合。
#[derive(Default, Clone, Copy)]
struct ZAddOptions {
    nx: bool,
    xx: bool,
    gt: bool,
    lt: bool,
    ch: bool,
    incr: bool,
}

/// 解析 `ZADD` 的选项，返回选项与第一个分数所在的下标。
///
/// 互斥规则与真机一致：`NX` 与 `XX` 不能同时出现，`GT` / `LT` 不能与 `NX`
/// 同时出现，`GT` 与 `LT` 也互斥。选项之后必须剩下一对或以上的分数与成员。
fn parse_zadd_options(arguments: &[&[u8]]) -> Result<(ZAddOptions, usize), CommandError> {
    let mut options = ZAddOptions::default();
    let mut index = 2;
    while index < arguments.len() {
        let argument = arguments[index];
        if argument.eq_ignore_ascii_case(b"NX") {
            options.nx = true;
        }
        else if argument.eq_ignore_ascii_case(b"XX") {
            options.xx = true;
        }
        else if argument.eq_ignore_ascii_case(b"GT") {
            options.gt = true;
        }
        else if argument.eq_ignore_ascii_case(b"LT") {
            options.lt = true;
        }
        else if argument.eq_ignore_ascii_case(b"CH") {
            options.ch = true;
        }
        else if argument.eq_ignore_ascii_case(b"INCR") {
            options.incr = true;
        }
        else {
            break;
        }
        index += 1;
    }
    if options.nx && options.xx {
        return Err(CommandError::Failed("XX and NX options at the same time are not compatible".to_owned()));
    }
    if (options.gt || options.lt) && (options.nx || (options.gt && options.lt)) {
        return Err(CommandError::Failed("GT, LT, and/or NX options at the same time are not compatible".to_owned()));
    }
    let remaining = arguments.len() - index;
    if remaining == 0 || remaining % 2 != 0 {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    if options.incr && remaining != 2 {
        return Err(CommandError::Failed("INCR option supports a single increment-element pair".to_owned()));
    }
    Ok((options, index))
}

/// 解析 `ZRANGEBYSCORE` 的 `WITHSCORES` 与 `LIMIT offset count` 选项。
fn parse_score_range_options(arguments: &[&[u8]], start: usize) -> Result<(bool, Option<(i64, i64)>), CommandError> {
    let mut with_scores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut index = start;
    while index < arguments.len() {
        if arguments[index].eq_ignore_ascii_case(b"WITHSCORES") {
            with_scores = true;
            index += 1;
        }
        else if arguments[index].eq_ignore_ascii_case(b"LIMIT") {
            if index + 2 >= arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            let (Some(offset), Some(count)) = (parse_index(arguments[index + 1]), parse_index(arguments[index + 2]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            limit = Some((offset, count));
            index += 3;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    Ok((with_scores, limit))
}

/// 解析 `ZRANGEBYLEX` 的 `LIMIT offset count` 选项，字典族不接受 `WITHSCORES`。
fn parse_lex_options(arguments: &[&[u8]], start: usize) -> Result<Option<(i64, i64)>, CommandError> {
    let mut limit: Option<(i64, i64)> = None;
    let mut index = start;
    while index < arguments.len() {
        if arguments[index].eq_ignore_ascii_case(b"LIMIT") && index + 2 < arguments.len() {
            let (Some(offset), Some(count)) = (parse_index(arguments[index + 1]), parse_index(arguments[index + 2]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            limit = Some((offset, count));
            index += 3;
        }
        else if arguments[index].eq_ignore_ascii_case(b"WITHSCORES") {
            // 真机在字典族上拒绝分数输出，并给出专门的提示。
            return Err(CommandError::Failed("syntax error, WITHSCORES not supported in combination with BYLEX".to_owned()));
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    Ok(limit)
}

/// 一段 hash 扫描结果：`[next-cursor, [field, value, ...]]`。
fn hscan_page_reply(fields: &[(Vec<u8>, Vec<u8>)], cursor: usize, count: usize, pattern: Option<&[u8]>) -> Reply {
    if cursor >= fields.len() {
        return Reply::array(vec![Reply::bulk(b"0".to_vec()), Reply::array(vec![])]);
    }
    let end = cursor.saturating_add(count).min(fields.len());
    let next = if end >= fields.len() { "0".to_string() } else { end.to_string() };
    let matched: Vec<Reply> = fields[cursor..end]
        .iter()
        .filter(|(field, _)| pattern.map_or(true, |pattern| glob_match(pattern, field)))
        .flat_map(|(field, value)| [Reply::bulk(field.clone()), Reply::bulk(value.clone())])
        .collect();
    Reply::array(vec![Reply::bulk(next.into_bytes()), Reply::array(matched)])
}

/// 一段 zset 扫描结果：`[next-cursor, [member, score, ...]]`。
fn zscan_page_reply(entries: &[(Vec<u8>, f64)], cursor: usize, count: usize, pattern: Option<&[u8]>) -> Reply {
    if cursor >= entries.len() {
        return Reply::array(vec![Reply::bulk(b"0".to_vec()), Reply::array(vec![])]);
    }
    let end = cursor.saturating_add(count).min(entries.len());
    let next = if end >= entries.len() { "0".to_string() } else { end.to_string() };
    let matched: Vec<Reply> = entries[cursor..end]
        .iter()
        .filter(|(member, _)| pattern.map_or(true, |pattern| glob_match(pattern, member)))
        .flat_map(|(member, score)| {
            // 真机的 `ZSCAN` 在 RESP3 下仍把分数作为 bulk 文本。
            [Reply::bulk(member.clone()), Reply::bulk(score_text(*score).into_bytes())]
        })
        .collect();
    Reply::array(vec![Reply::bulk(next.into_bytes()), Reply::array(matched)])
}

/// zset 集合运算的方式。
#[derive(Clone, Copy, PartialEq)]
enum ZSetOp {
    Union,
    Inter,
    Diff,
}

/// zset 同一成员在多键出现时的聚合方式，默认 `SUM`。
#[derive(Clone, Copy, PartialEq)]
enum ZSetAggregate {
    Sum,
    Min,
    Max,
}

impl ZSetAggregate {
    fn combine(self, left: f64, right: f64) -> f64 {
        let value = match self {
            ZSetAggregate::Sum => left + right,
            ZSetAggregate::Min => left.min(right),
            ZSetAggregate::Max => left.max(right),
        };
        // 真机把聚合得到的 NaN 归零，避免非有限值进入编码层。
        if value.is_nan() { 0.0 } else { value }
    }
}

/// `ZUNIONSTORE` / `ZINTERSTORE` 解析后的键、权重与聚合方式。
struct ZSetStorePlan {
    keys: Vec<Vec<u8>>,
    weights: Vec<f64>,
    aggregate: ZSetAggregate,
}

fn parse_positive_usize(raw: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(raw).ok()?;
    let value = text.parse::<i64>().ok()?;
    (value > 0).then_some(value as usize)
}

/// 解析 `ZUNIONSTORE` / `ZINTERSTORE` 的 `numkeys`、键列表与可选选项。
fn parse_zset_store_plan(name: &str, arguments: &[&[u8]]) -> Result<ZSetStorePlan, CommandError> {
    let Some(numkeys) = parse_positive_usize(arguments[2])
    else {
        return Err(CommandError::Failed(format!("at least 1 input key is needed for '{name}' command").to_owned()));
    };
    if arguments.len() < 3 + numkeys {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let keys = arguments[3..3 + numkeys].iter().map(|key| key.to_vec()).collect();
    let mut weights = vec![1.0_f64; numkeys];
    let mut aggregate = ZSetAggregate::Sum;
    let mut index = 3 + numkeys;
    while index < arguments.len() {
        if arguments[index].eq_ignore_ascii_case(b"WEIGHTS") {
            if index + numkeys >= arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            for slot in 0..numkeys {
                let Some(weight) = parse_score(arguments[index + 1 + slot])
                else {
                    return Err(CommandError::Failed("weight value is not a float".to_owned()));
                };
                weights[slot] = weight;
            }
            index += 1 + numkeys;
        }
        else if arguments[index].eq_ignore_ascii_case(b"AGGREGATE") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            if raw.eq_ignore_ascii_case(b"SUM") {
                aggregate = ZSetAggregate::Sum;
            }
            else if raw.eq_ignore_ascii_case(b"MIN") {
                aggregate = ZSetAggregate::Min;
            }
            else if raw.eq_ignore_ascii_case(b"MAX") {
                aggregate = ZSetAggregate::Max;
            }
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            index += 2;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    Ok(ZSetStorePlan { keys, weights, aggregate })
}

/// 读取多个 zset 并校验类型；任一键类型不合法时整条命令按真机回 `WRONGTYPE`。
fn load_zsets(executor: &dyn KeyValueExecutor, base: &Namespace, keys: &[Vec<u8>]) -> Result<Vec<Vec<(Vec<u8>, f64)>>, CommandError> {
    let mut sets = Vec::with_capacity(keys.len());
    for key in keys {
        match load_zset(executor, base, key)? {
            Loaded::Missing => sets.push(Vec::new()),
            Loaded::Found(entries) => sets.push(entries),
        }
    }
    Ok(sets)
}

/// 按真机语义计算并集、交集或差集，结果按分数升序、同分按成员字节排序。
fn combine_zsets(sets: &[Vec<(Vec<u8>, f64)>], weights: &[f64], aggregate: ZSetAggregate, op: ZSetOp) -> Vec<(Vec<u8>, f64)> {
    let weighted = |score: f64, weight: f64| {
        let value = score * weight;
        if value.is_nan() { 0.0 } else { value }
    };
    let lookup = |set: &Vec<(Vec<u8>, f64)>, member: &[u8]| -> Option<f64> {
        set.iter().find(|(candidate, _)| candidate.as_slice() == member).map(|(_, score)| *score)
    };
    let mut result: Vec<(Vec<u8>, f64)> = Vec::new();
    match op {
        ZSetOp::Union => {
            for (index, set) in sets.iter().enumerate() {
                let weight = weights.get(index).copied().unwrap_or(1.0);
                for (member, score) in set {
                    let value = weighted(*score, weight);
                    match result.iter_mut().find(|(existing, _)| existing == member) {
                        Some(entry) => entry.1 = aggregate.combine(entry.1, value),
                        None => result.push((member.clone(), value)),
                    }
                }
            }
        }
        ZSetOp::Inter => {
            let Some(first) = sets.first()
            else {
                return Vec::new();
            };
            for (member, score) in first {
                let mut combined = weighted(*score, weights.first().copied().unwrap_or(1.0));
                let mut present = true;
                for (index, set) in sets.iter().enumerate().skip(1) {
                    let Some(other) = lookup(set, member)
                    else {
                        present = false;
                        break;
                    };
                    combined = aggregate.combine(combined, weighted(other, weights.get(index).copied().unwrap_or(1.0)));
                }
                if present {
                    result.push((member.clone(), combined));
                }
            }
        }
        ZSetOp::Diff => {
            let Some(first) = sets.first()
            else {
                return Vec::new();
            };
            for (member, score) in first {
                if !sets.iter().skip(1).any(|set| lookup(set, member).is_some()) {
                    result.push((member.clone(), *score));
                }
            }
        }
    }
    sort_zset(&mut result);
    result
}

/// 覆盖写入 `*STORE` 目标：先清掉全部类型表示，空结果直接删除目标。
fn store_zset_result(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    destination: &[u8],
    entries: &[(Vec<u8>, f64)],
) -> Result<Reply, CommandError> {
    if let Err(message) = remove_all_kinds(executor, base, destination) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    if entries.is_empty() {
        return Ok(Reply::integer(0));
    }
    let Some(bytes) = encode_zset(entries)
    else {
        return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
    };
    if let Err(message) = store_fresh_value(executor, &KeyKind::ZSet.namespace(base), destination, bytes) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    Ok(Reply::integer(entries.len() as i64))
}

/// 解析 `ZUNION` / `ZINTER` 的键、权重、聚合方式与 `WITHSCORES`。
fn parse_zset_combine_plan(name: &str, arguments: &[&[u8]], numkeys_index: usize) -> Result<(ZSetStorePlan, bool), CommandError> {
    let Some(numkeys) = parse_positive_usize(arguments[numkeys_index])
    else {
        return Err(CommandError::Failed(format!("at least 1 input key is needed for '{name}' command").to_owned()));
    };
    let keys_start = numkeys_index + 1;
    if arguments.len() < keys_start + numkeys {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let keys = arguments[keys_start..keys_start + numkeys].iter().map(|key| key.to_vec()).collect();
    let mut weights = vec![1.0_f64; numkeys];
    let mut aggregate = ZSetAggregate::Sum;
    let mut with_scores = false;
    let mut index = keys_start + numkeys;
    while index < arguments.len() {
        if arguments[index].eq_ignore_ascii_case(b"WEIGHTS") {
            if index + numkeys >= arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            for slot in 0..numkeys {
                let Some(weight) = parse_score(arguments[index + 1 + slot])
                else {
                    return Err(CommandError::Failed("weight value is not a float".to_owned()));
                };
                weights[slot] = weight;
            }
            index += 1 + numkeys;
        }
        else if arguments[index].eq_ignore_ascii_case(b"AGGREGATE") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            if raw.eq_ignore_ascii_case(b"SUM") {
                aggregate = ZSetAggregate::Sum;
            }
            else if raw.eq_ignore_ascii_case(b"MIN") {
                aggregate = ZSetAggregate::Min;
            }
            else if raw.eq_ignore_ascii_case(b"MAX") {
                aggregate = ZSetAggregate::Max;
            }
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            index += 2;
        }
        else if arguments[index].eq_ignore_ascii_case(b"WITHSCORES") {
            with_scores = true;
            index += 1;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    Ok((ZSetStorePlan { keys, weights, aggregate }, with_scores))
}

/// 校验阻塞命令的 `timeout` 参数，返回真机的错误文案。
fn validate_timeout(raw: &[u8]) -> Option<&'static str> {
    classify_timeout(raw).err()
}

/// 取一个 `0..bound` 的伪随机下标。
///
/// `RandomState` 每次构造都带随机种子，足以满足 `ZRANDMEMBER` 的随机性要求，
/// 又不引入新的依赖。
fn random_below(bound: usize) -> usize {
    if bound == 0 {
        return 0;
    }
    let state = std::collections::hash_map::RandomState::new();
    let mut hasher = std::hash::BuildHasher::build_hasher(&state);
    std::hash::Hasher::write_usize(&mut hasher, bound);
    (std::hash::Hasher::finish(&hasher) % bound as u64) as usize
}

/// 无放回抽取最多 `count` 个成员，顺序随机。
fn sample_distinct(entries: &[(Vec<u8>, f64)], count: usize) -> Vec<(Vec<u8>, f64)> {
    let take = count.min(entries.len());
    let mut indices: Vec<usize> = (0..entries.len()).collect();
    for slot in 0..take {
        let pick = slot + random_below(entries.len() - slot);
        indices.swap(slot, pick);
    }
    indices[..take].iter().map(|index| entries[*index].clone()).collect()
}

/// `ZMPOP` / `BZMPOP` 的非阻塞实现，`numkeys` 位于 `numkeys_index`。
fn handle_zset_mpop(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    arguments: &[&[u8]],
    numkeys_index: usize,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let Some(numkeys) = parse_positive_usize(arguments[numkeys_index])
    else {
        return Err(CommandError::Failed("numkeys should be greater than 0".to_owned()));
    };
    let keys_start = numkeys_index + 1;
    if arguments.len() < keys_start + numkeys + 1 {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let keys = &arguments[keys_start..keys_start + numkeys];
    let mut index = keys_start + numkeys;
    let maximum = if arguments[index].eq_ignore_ascii_case(b"MAX") {
        true
    }
    else if arguments[index].eq_ignore_ascii_case(b"MIN") {
        false
    }
    else {
        return Err(CommandError::Failed("syntax error".to_owned()));
    };
    index += 1;
    let mut count = 1_usize;
    if index < arguments.len() {
        if !arguments[index].eq_ignore_ascii_case(b"COUNT") || index + 1 >= arguments.len() {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let Some(value) = parse_index(arguments[index + 1])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        if value <= 0 {
            return Err(CommandError::Failed("count should be greater than 0".to_owned()));
        }
        count = value as usize;
        index += 2;
    }
    if index != arguments.len() {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    for key in keys {
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => continue,
            Loaded::Found(entries) => entries,
        };
        if entries.is_empty() {
            continue;
        }
        sort_zset(&mut entries);
        if maximum {
            entries.reverse();
        }
        let take = count.min(entries.len());
        let popped = entries[..take].to_vec();
        let remaining = entries[take..].to_vec();
        if remaining.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::ZSet.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_zset(&remaining)
            else {
                return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::array(vec![Reply::bulk(key.to_vec()), zset_pairs_reply(&popped, protocol)]));
    }
    Ok(Reply::nil_array())
}

/// 解析 `ZDIFF` / `ZDIFFSTORE` 的 `numkeys` 与键列表，`numkeys` 位于 `numkeys_index`。
fn parse_diff_keys(arguments: &[&[u8]], name: &str, numkeys_index: usize) -> Result<(Vec<Vec<u8>>, usize), CommandError> {
    let Some(numkeys) = parse_positive_usize(arguments[numkeys_index])
    else {
        return Err(CommandError::Failed(format!("at least 1 input key is needed for '{name}' command").to_owned()));
    };
    if arguments.len() < numkeys_index + 1 + numkeys {
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let keys = arguments[numkeys_index + 1..numkeys_index + 1 + numkeys].iter().map(|key| key.to_vec()).collect();
    Ok((keys, numkeys_index + 1 + numkeys))
}

/// `SORT` / `SORT_RO` 解析后的选项。
struct SortOptions {
    by: Option<Vec<u8>>,
    limit: Option<(usize, Option<usize>)>,
    get: Vec<Vec<u8>>,
    descending: bool,
    alpha: bool,
    store: Option<Vec<u8>>,
}

/// 解析 `SORT` 的 `BY` / `LIMIT` / `GET` / `ASC|DESC` / `ALPHA` / `STORE`。
fn parse_sort_options(arguments: &[&[u8]]) -> Result<SortOptions, CommandError> {
    let mut options = SortOptions { by: None, limit: None, get: Vec::new(), descending: false, alpha: false, store: None };
    let mut index = 2;
    while index < arguments.len() {
        let option = arguments[index];
        if option.eq_ignore_ascii_case(b"BY") {
            let Some(pattern) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            options.by = Some(pattern.to_vec());
            index += 2;
        }
        else if option.eq_ignore_ascii_case(b"LIMIT") {
            if index + 2 >= arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            let (Some(offset), Some(count)) = (parse_index(arguments[index + 1]), parse_index(arguments[index + 2]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            options.limit = Some((offset.max(0) as usize, (count >= 0).then_some(count as usize)));
            index += 3;
        }
        else if option.eq_ignore_ascii_case(b"GET") {
            let Some(pattern) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            options.get.push(pattern.to_vec());
            index += 2;
        }
        else if option.eq_ignore_ascii_case(b"ASC") {
            options.descending = false;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"DESC") {
            options.descending = true;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"ALPHA") {
            options.alpha = true;
            index += 1;
        }
        else if option.eq_ignore_ascii_case(b"STORE") {
            let Some(destination) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            options.store = Some(destination.to_vec());
            index += 2;
        }
        else {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
    }
    Ok(options)
}

/// 读取 `SORT` 的源元素；list 保持顺序，set 与 zset 按真机的遍历顺序（zset 按分数）。
fn load_sort_elements(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<Vec<Vec<u8>>, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => Ok(Vec::new()),
        Ok(Some(KeyKind::List)) => match load_list(executor, base, key)? {
            Loaded::Found(elements) => Ok(elements),
            Loaded::Missing => Ok(Vec::new()),
        },
        Ok(Some(KeyKind::Set)) => match load_set(executor, base, key)? {
            Loaded::Found(members) => Ok(members),
            Loaded::Missing => Ok(Vec::new()),
        },
        Ok(Some(KeyKind::ZSet)) => match load_zset(executor, base, key)? {
            Loaded::Found(mut entries) => {
                sort_zset(&mut entries);
                Ok(entries.into_iter().map(|(member, _)| member).collect())
            }
            Loaded::Missing => Ok(Vec::new()),
        },
        Ok(Some(_)) => Err(CommandError::WrongType),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// 按 `BY` / `GET` 模式取值：`*` 被元素替换，`->` 后缀表示哈希字段，类型不匹配视为缺失。
fn lookup_sort_pattern(executor: &dyn KeyValueExecutor, base: &Namespace, pattern: &[u8], element: &[u8]) -> Option<Vec<u8>> {
    let mut expanded = Vec::new();
    for byte in pattern {
        if *byte == b'*' {
            expanded.extend_from_slice(element);
        }
        else {
            expanded.push(*byte);
        }
    }
    let (key, field) = match expanded.windows(2).position(|window| window == b"->") {
        Some(position) => (&expanded[..position], Some(&expanded[position + 2..])),
        None => (expanded.as_slice(), None),
    };
    match (find_kind(executor, base, key), field) {
        (Ok(Some(KeyKind::String)), None) => load_value(executor, &KeyKind::String.namespace(base), key).ok().flatten(),
        (Ok(Some(KeyKind::Hash)), Some(field)) => load_value(executor, &KeyKind::Hash.namespace(base), key)
            .ok()
            .flatten()
            .and_then(|bytes| decode_hash(&bytes))
            .and_then(|fields| fields.into_iter().find(|(candidate, _)| candidate == field).map(|(_, value)| value)),
        _ => None,
    }
}

/// 排序键：`ALPHA` 用字节序，否则为浮点分数。
enum SortKey {
    Text(Vec<u8>),
    Number(f64),
}

/// 执行 `SORT` / `SORT_RO`。
fn handle_sort(executor: &dyn KeyValueExecutor, base: &Namespace, arguments: &[&[u8]], readonly: bool) -> Result<Reply, CommandError> {
    let name = if readonly { "sort_ro" } else { "sort" };
    if arguments.len() < 2 {
        return Err(arity_error(name));
    }
    let options = parse_sort_options(arguments)?;
    if readonly && options.store.is_some() {
        // 真机对 `SORT_RO ... STORE` 直接回答通用语法错误。
        return Err(CommandError::Failed("syntax error".to_owned()));
    }
    let key = arguments[1];
    let elements = load_sort_elements(executor, base, key)?;
    let no_sort = options.by.as_ref().is_some_and(|pattern| !pattern.contains(&b'*'));
    let mut entries: Vec<(Vec<u8>, Option<SortKey>)> = Vec::with_capacity(elements.len());
    for element in elements {
        let looked_up = match &options.by {
            Some(pattern) => lookup_sort_pattern(executor, base, pattern, &element),
            None => Some(element.clone()),
        };
        let sort_key = if no_sort {
            None
        }
        else if options.alpha {
            Some(SortKey::Text(looked_up.unwrap_or_default()))
        }
        else {
            match looked_up {
                Some(raw) => match parse_score(&raw) {
                    Some(value) => Some(SortKey::Number(value)),
                    None => {
                        return Err(CommandError::Failed("One or more scores can't be converted into double".to_owned()));
                    }
                },
                None => Some(SortKey::Number(0.0)),
            }
        };
        entries.push((element, sort_key));
    }
    if !no_sort {
        entries.sort_by(|left, right| {
            let order = match (&left.1, &right.1) {
                (Some(SortKey::Text(left_key)), Some(SortKey::Text(right_key))) => left_key.cmp(right_key),
                (Some(SortKey::Number(left_key)), Some(SortKey::Number(right_key))) => {
                    left_key.partial_cmp(right_key).unwrap_or(std::cmp::Ordering::Equal)
                }
                _ => std::cmp::Ordering::Equal,
            };
            if options.descending { order.reverse() } else { order }
        });
    }
    let (offset, count) = options.limit.unwrap_or((0, None));
    let selected: Vec<&Vec<u8>> = entries.iter().skip(offset).map(|(element, _)| element).take(count.unwrap_or(usize::MAX)).collect();
    let gather = |element: &Vec<u8>| -> Vec<Option<Vec<u8>>> {
        if options.get.is_empty() {
            return vec![Some(element.clone())];
        }
        options
            .get
            .iter()
            .map(
                |pattern| {
                    if pattern.as_slice() == b"#" { Some(element.clone()) } else { lookup_sort_pattern(executor, base, pattern, element) }
                },
            )
            .collect()
    };
    let values: Vec<Option<Vec<u8>>> = selected.iter().flat_map(|element| gather(element)).collect();
    if let Some(destination) = &options.store {
        if let Err(message) = remove_all_kinds(executor, base, destination) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        if values.is_empty() {
            return Ok(Reply::integer(0));
        }
        let stored: Vec<Vec<u8>> = values.into_iter().map(|value| value.unwrap_or_default()).collect();
        let Some(bytes) = encode_list(&stored)
        else {
            return Err(CommandError::Failed("list exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_fresh_value(executor, &KeyKind::List.namespace(base), destination, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(Reply::integer(stored.len() as i64));
    }
    let items: Vec<Reply> = values.into_iter().map(|value| value.map_or_else(Reply::nil, Reply::bulk)).collect();
    Ok(Reply::array(items))
}

/// 位域编码：有符号 `iN` 或无符号 `uN`。
#[derive(Clone, Copy)]
struct BitEncoding {
    signed: bool,
    bits: u32,
}

/// 位域溢出策略，默认 `WRAP`。
#[derive(Clone, Copy, PartialEq)]
enum BitOverflow {
    Wrap,
    Sat,
    Fail,
}

/// 一条 `BITFIELD` 子命令。
enum BitFieldOp {
    Get { encoding: BitEncoding, offset: u64 },
    Set { encoding: BitEncoding, offset: u64, value: i64, overflow: BitOverflow },
    Incr { encoding: BitEncoding, offset: u64, increment: i64, overflow: BitOverflow },
}

const MAX_BIT_OFFSET: u64 = 0xffff_ffff;

/// 解析 `iN` / `uN`，无符号上限 `u63`，有符号上限 `i64`。
///
/// 真机只认小写前缀与规范十进制，`I8`、`i08`、`i+8` 一律按非法编码报错。
fn parse_bit_encoding(raw: &[u8]) -> Option<BitEncoding> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut characters = text.chars();
    let signed = match characters.next()? {
        'i' => true,
        'u' => false,
        _ => return None,
    };
    let digits = characters.as_str();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let bits = digits.parse::<u32>().ok()?;
    let limit = if signed { 64 } else { 63 };
    (bits >= 1 && bits <= limit).then_some(BitEncoding { signed, bits })
}

/// 解析偏移：纯数字按位计，`#n` 按编码宽度倍乘。
///
/// 偏移与真机一样按规范整数校验，`+8`、`08`、`#+1` 都算越界参数。
fn parse_bitfield_offset(raw: &[u8], bits: u32) -> Option<u64> {
    let text = std::str::from_utf8(raw).ok()?;
    let value = match text.strip_prefix('#') {
        Some(rest) => parse_index(rest.as_bytes())?.checked_mul(bits as i64)?,
        None => parse_index(raw)?,
    };
    (value >= 0 && value as u64 <= MAX_BIT_OFFSET).then_some(value as u64)
}

/// 解析整条 `BITFIELD`：语法与编码校验先于任何写入。
fn parse_bitfield_ops(arguments: &[&[u8]], readonly: bool) -> Result<Vec<BitFieldOp>, CommandError> {
    let mut operations = Vec::new();
    let mut overflow = BitOverflow::Wrap;
    let mut index = 2;
    while index < arguments.len() {
        let keyword = arguments[index];
        if keyword.eq_ignore_ascii_case(b"OVERFLOW") {
            let Some(raw) = arguments.get(index + 1)
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            };
            if raw.eq_ignore_ascii_case(b"WRAP") {
                overflow = BitOverflow::Wrap;
            }
            else if raw.eq_ignore_ascii_case(b"SAT") {
                overflow = BitOverflow::Sat;
            }
            else if raw.eq_ignore_ascii_case(b"FAIL") {
                overflow = BitOverflow::Fail;
            }
            else {
                return Err(CommandError::Failed("Invalid OVERFLOW type specified".to_owned()));
            }
            index += 2;
            continue;
        }
        let writing = keyword.eq_ignore_ascii_case(b"SET") || keyword.eq_ignore_ascii_case(b"INCRBY");
        if readonly && writing {
            return Err(CommandError::Failed("BITFIELD_RO only supports the GET subcommand".to_owned()));
        }
        let needs = if keyword.eq_ignore_ascii_case(b"GET") {
            3
        }
        else if writing {
            4
        }
        else {
            0
        };
        if needs == 0 {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        if index + needs > arguments.len() {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let Some(encoding) = parse_bit_encoding(arguments[index + 1])
        else {
            return Err(CommandError::Failed(
                "Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.".to_owned(),
            ));
        };
        let Some(offset) = parse_bitfield_offset(arguments[index + 2], encoding.bits)
        else {
            return Err(CommandError::Failed("bit offset is not an integer or out of range".to_owned()));
        };
        if keyword.eq_ignore_ascii_case(b"GET") {
            operations.push(BitFieldOp::Get { encoding, offset });
        }
        else {
            let Some(value) = parse_index(arguments[index + 3])
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            if keyword.eq_ignore_ascii_case(b"SET") {
                operations.push(BitFieldOp::Set { encoding, offset, value, overflow });
            }
            else {
                operations.push(BitFieldOp::Incr { encoding, offset, increment: value, overflow });
            }
        }
        index += needs;
    }
    Ok(operations)
}

/// 读取一个位域值，缺失的位按 0 参与。
fn read_bitfield(body: &[u8], offset: u64, encoding: BitEncoding) -> i128 {
    let mut raw = 0_u128;
    for step in 0..encoding.bits as u64 {
        let position = offset + step;
        let byte = (position / 8) as usize;
        let bit = if byte < body.len() { (body[byte] >> (7 - (position % 8))) & 1 } else { 0 };
        raw = (raw << 1) | bit as u128;
    }
    if encoding.signed && raw >= (1_u128 << (encoding.bits - 1)) { raw as i128 - (1_i128 << encoding.bits) } else { raw as i128 }
}

/// 把位域末端补齐到完整字节。
///
/// 真机连 `FAIL` 分支也会建键并补零，所以扩容与是否真正落值必须拆开。
fn ensure_bitfield_len(body: &mut Vec<u8>, offset: u64, bits: u32) {
    let needed = ((offset + bits as u64 + 7) / 8) as usize;
    if body.len() < needed {
        body.resize(needed, 0);
    }
}

/// 写入一个位域值，必要时按位增长并补零。
fn write_bitfield(body: &mut Vec<u8>, offset: u64, encoding: BitEncoding, value: i128) {
    let bits = encoding.bits;
    let mask = (1_u128 << bits) - 1;
    let raw = (value as u128) & mask;
    ensure_bitfield_len(body, offset, bits);
    for step in 0..bits as u64 {
        let position = offset + step;
        let byte = (position / 8) as usize;
        let bit = ((raw >> (bits as u64 - 1 - step)) & 1) as u8;
        let shift = 7 - (position % 8);
        if bit == 1 {
            body[byte] |= 1 << shift;
        }
        else {
            body[byte] &= !(1 << shift);
        }
    }
}

/// 位域的取值区间与溢出处理；`FAIL` 返回 `None`。
fn normalize_bitfield(value: i128, encoding: BitEncoding, overflow: BitOverflow) -> Option<i128> {
    let (minimum, maximum) = if encoding.signed {
        (-(1_i128 << (encoding.bits - 1)), (1_i128 << (encoding.bits - 1)) - 1)
    }
    else {
        (0, (1_i128 << encoding.bits) - 1)
    };
    if value >= minimum && value <= maximum {
        return Some(value);
    }
    match overflow {
        BitOverflow::Fail => None,
        BitOverflow::Sat => Some(value.clamp(minimum, maximum)),
        BitOverflow::Wrap => {
            let span = maximum - minimum + 1;
            Some(minimum + ((value - minimum) % span + span) % span)
        }
    }
}

/// `SET` 的取值规整。
///
/// 真机在 `SET` 上同样应用 `OVERFLOW`，但无符号编码把负数当成 `u64` 视角的超大值，
/// 因此 `SAT` 落到上界、`FAIL` 直接失败，而不是按有符号下限裁剪到 0。
fn normalize_set_bitfield(value: i64, encoding: BitEncoding, overflow: BitOverflow) -> Option<i128> {
    if encoding.signed {
        return normalize_bitfield(value as i128, encoding, overflow);
    }
    let maximum = (1_i128 << encoding.bits) - 1;
    let raw = if value < 0 { (1_i128 << 64) + value as i128 } else { value as i128 };
    if raw <= maximum {
        return Some(raw);
    }
    match overflow {
        BitOverflow::Fail => None,
        BitOverflow::Sat => Some(maximum),
        BitOverflow::Wrap => Some(raw & maximum),
    }
}

/// 执行 `BITFIELD` / `BITFIELD_RO`。
pub fn handle_bitfield(
    executor: &dyn KeyValueExecutor,
    base: &Namespace,
    arguments: &[&[u8]],
    readonly: bool,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let name = if readonly { "bitfield_ro" } else { "bitfield" };
    if arguments.len() < 2 {
        return Err(arity_error(name));
    }
    let key = arguments[1];
    let operations = parse_bitfield_ops(arguments, readonly)?;
    let kind = match find_kind(executor, base, key) {
        Ok(kind) => kind,
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    };
    if let Some(found) = kind {
        if found != KeyKind::String {
            return Err(CommandError::WrongType);
        }
    }
    let mut body = match load_value(executor, &KeyKind::String.namespace(base), key) {
        Ok(value) => value.unwrap_or_default(),
        Err(message) => return Err(CommandError::Failed(message.to_owned())),
    };
    let mut results: Vec<Option<i128>> = Vec::with_capacity(operations.len());
    let mut changed = false;
    for operation in operations {
        match operation {
            BitFieldOp::Get { encoding, offset } => results.push(Some(read_bitfield(&body, offset, encoding))),
            BitFieldOp::Set { encoding, offset, value, overflow } => {
                let previous = read_bitfield(&body, offset, encoding);
                ensure_bitfield_len(&mut body, offset, encoding.bits);
                // 真机对 SET 也应用当前 OVERFLOW，失败时同样建键补零。
                changed = true;
                match normalize_set_bitfield(value, encoding, overflow) {
                    Some(written) => {
                        write_bitfield(&mut body, offset, encoding, written);
                        results.push(Some(previous));
                    }
                    None => results.push(None),
                }
            }
            BitFieldOp::Incr { encoding, offset, increment, overflow } => {
                let previous = read_bitfield(&body, offset, encoding);
                ensure_bitfield_len(&mut body, offset, encoding.bits);
                changed = true;
                match normalize_bitfield(previous + increment as i128, encoding, overflow) {
                    Some(value) => {
                        write_bitfield(&mut body, offset, encoding, value);
                        results.push(Some(value));
                    }
                    None => results.push(None),
                }
            }
        }
    }
    if changed {
        if let Err(message) = store_value(executor, &KeyKind::String.namespace(base), key, body) {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    let items: Vec<Reply> = results.into_iter().map(|value| value.map_or_else(Reply::nil, |value| Reply::integer(value as i64))).collect();
    Ok(Reply::array(items))
}

/// `ZADD` / `ZSCORE` / `ZREM` / `ZCARD` / `ZRANGE` 及排序族、排名、计数与扫描的分派入口。
pub fn handle_zset(
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    base: &Namespace,
    protocol: u8,
) -> Result<Reply, CommandError> {
    let command = request.arguments[0];
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    if command.eq_ignore_ascii_case(b"SORT") || command.eq_ignore_ascii_case(b"SORT_RO") {
        return handle_sort(executor, base, &request.arguments, command.eq_ignore_ascii_case(b"SORT_RO"));
    }
    if command.eq_ignore_ascii_case(b"ZUNIONSTORE") || command.eq_ignore_ascii_case(b"ZINTERSTORE") {
        let union = command.eq_ignore_ascii_case(b"ZUNIONSTORE");
        if request.arguments.len() < 4 {
            return Err(arity_error(if union { "zunionstore" } else { "zinterstore" }));
        }
        let destination = request.arguments[1];
        let name = if union { "zunionstore" } else { "zinterstore" };
        let plan = parse_zset_store_plan(name, &request.arguments)?;
        let sets = load_zsets(executor, base, &plan.keys)?;
        let result = combine_zsets(&sets, &plan.weights, plan.aggregate, if union { ZSetOp::Union } else { ZSetOp::Inter });
        return store_zset_result(executor, base, destination, &result);
    }
    if command.eq_ignore_ascii_case(b"ZDIFF") || command.eq_ignore_ascii_case(b"ZDIFFSTORE") {
        let store = command.eq_ignore_ascii_case(b"ZDIFFSTORE");
        if store {
            if request.arguments.len() < 4 {
                return Err(arity_error("zdiffstore"));
            }
        }
        else if request.arguments.len() < 3 {
            return Err(arity_error("zdiff"));
        }
        let name = if store { "zdiffstore" } else { "zdiff" };
        let numkeys_index = if store { 2 } else { 1 };
        let (keys, next) = parse_diff_keys(&request.arguments, name, numkeys_index)?;
        let mut with_scores = false;
        if store {
            if next != request.arguments.len() {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
        }
        else {
            match request.arguments.len() - next {
                0 => {}
                1 if request.arguments[next].eq_ignore_ascii_case(b"WITHSCORES") => with_scores = true,
                _ => return Err(CommandError::Failed("syntax error".to_owned())),
            }
        }
        let sets = load_zsets(executor, base, &keys)?;
        let weights = vec![1.0_f64; keys.len()];
        let result = combine_zsets(&sets, &weights, ZSetAggregate::Sum, ZSetOp::Diff);
        if store {
            return store_zset_result(executor, base, request.arguments[1], &result);
        }
        return Ok(zset_entries_reply(&result, with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZINTERCARD") {
        if request.arguments.len() < 3 {
            return Err(arity_error("zintercard"));
        }
        let Some(numkeys) = parse_positive_usize(request.arguments[1])
        else {
            return Err(CommandError::Failed("at least 1 input key is needed for 'zintercard' command".to_owned()));
        };
        if request.arguments.len() < 2 + numkeys {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let keys: Vec<Vec<u8>> = request.arguments[2..2 + numkeys].iter().map(|key| key.to_vec()).collect();
        let mut limit: usize = 0;
        match request.arguments.len() - (2 + numkeys) {
            0 => {}
            2 if request.arguments[2 + numkeys].eq_ignore_ascii_case(b"LIMIT") => {
                // 真机对非整数与负数统一回复「不能为负」。
                let Some(value) = parse_index(request.arguments[3 + numkeys])
                else {
                    return Err(CommandError::Failed("LIMIT can't be negative".to_owned()));
                };
                if value < 0 {
                    return Err(CommandError::Failed("LIMIT can't be negative".to_owned()));
                }
                limit = value as usize;
            }
            _ => return Err(CommandError::Failed("syntax error".to_owned())),
        }
        let sets = load_zsets(executor, base, &keys)?;
        let mut cardinality = 0_usize;
        if let Some(first) = sets.first() {
            for (member, _) in first {
                if sets.iter().skip(1).all(|set| set.iter().any(|(candidate, _)| candidate == member)) {
                    cardinality += 1;
                    if limit > 0 && cardinality >= limit {
                        break;
                    }
                }
            }
        }
        return Ok(Reply::integer(cardinality as i64));
    }
    if command.eq_ignore_ascii_case(b"ZADD") {
        if request.arguments.len() < 4 {
            return Err(arity_error("zadd"));
        }
        let (options, start) = parse_zadd_options(&request.arguments)?;
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::ZSet)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut entries = match load_value(executor, &KeyKind::ZSet.namespace(base), key) {
            Ok(Some(bytes)) => match decode_zset(&bytes) {
                Some(entries) => entries,
                None => return Err(CommandError::Failed("stored zset value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let pairs = request.arguments[start..].chunks(2).collect::<Vec<_>>();
        // 全部分数先校验：任一非法都不写入，避免半截修改。
        let mut scores = Vec::with_capacity(pairs.len());
        for pair in &pairs {
            let Some(score) = parse_score(pair[0])
            else {
                return Err(CommandError::Failed("value is not a valid float".to_owned()));
            };
            scores.push(score);
        }
        let mut added = 0_i64;
        let mut changed = 0_i64;
        let mut increment_reply: Option<f64> = None;
        for (pair, score) in pairs.iter().zip(scores.iter().copied()) {
            let member = pair[1].to_vec();
            let current = entries.iter().position(|(existing, _)| *existing == member);
            match current {
                Some(index) => {
                    let previous = entries[index].1;
                    let next = if options.incr { previous + score } else { score };
                    if next.is_nan() {
                        return Err(CommandError::Failed("resulting score is not a number (NaN)".to_owned()));
                    }
                    let mut accepted = true;
                    if options.nx {
                        accepted = false;
                    }
                    if options.gt && next <= previous {
                        accepted = false;
                    }
                    if options.lt && next >= previous {
                        accepted = false;
                    }
                    if accepted {
                        if next != previous {
                            entries[index].1 = next;
                            changed += 1;
                        }
                        if options.incr {
                            increment_reply = Some(next);
                        }
                    }
                    else if options.incr {
                        increment_reply = None;
                    }
                }
                None => {
                    if options.xx {
                        continue;
                    }
                    entries.push((member, score));
                    added += 1;
                    if options.incr {
                        increment_reply = Some(score);
                    }
                }
            }
        }
        let touched = added > 0 || changed > 0;
        if touched {
            let Some(bytes) = encode_zset(&entries)
            else {
                return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        // 没有改动时不重写记录，避免平白增加一次写入。
        if options.incr {
            return match increment_reply {
                Some(score) => Ok(score_reply(score)),
                None => Ok(Reply::nil()),
            };
        }
        return Ok(Reply::integer(if options.ch { added + changed } else { added }));
    }
    if command.eq_ignore_ascii_case(b"ZSCORE") {
        if request.arguments.len() != 3 {
            return Err(arity_error("zscore"));
        }
        let key = request.arguments[1];
        return match load_zset(executor, base, key)? {
            Loaded::Missing => Ok(Reply::nil()),
            Loaded::Found(entries) => match entries.iter().find(|(member, _)| member.as_slice() == request.arguments[2]) {
                Some((_, score)) => Ok(score_reply(*score)),
                None => Ok(Reply::nil()),
            },
        };
    }
    if command.eq_ignore_ascii_case(b"ZREM") {
        if request.arguments.len() < 3 {
            return Err(arity_error("zrem"));
        }
        let key = request.arguments[1];
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(entries) => entries,
        };
        let mut removed = 0_i64;
        entries.retain(|(member, _)| {
            let hit = request.arguments[2..].iter().any(|candidate| candidate == member);
            if hit {
                removed += 1;
            }
            !hit
        });
        if entries.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::ZSet.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_zset(&entries)
            else {
                return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(removed));
    }
    if command.eq_ignore_ascii_case(b"ZCARD") {
        if request.arguments.len() != 2 {
            return Err(arity_error("zcard"));
        }
        let key = request.arguments[1];
        return match load_zset(executor, base, key)? {
            Loaded::Missing => Ok(Reply::integer(0)),
            Loaded::Found(entries) => Ok(Reply::integer(entries.len() as i64)),
        };
    }
    if command.eq_ignore_ascii_case(b"ZRANGEBYSCORE") || command.eq_ignore_ascii_case(b"ZREVRANGEBYSCORE") {
        let reverse = command.eq_ignore_ascii_case(b"ZREVRANGEBYSCORE");
        if request.arguments.len() < 4 {
            let name = if reverse { "zrevrangebyscore" } else { "zrangebyscore" };
            return Err(arity_error(name));
        }
        let key = request.arguments[1];
        let (high, low) = if reverse { (request.arguments[2], request.arguments[3]) } else { (request.arguments[3], request.arguments[2]) };
        let (Some(min), Some(max)) = (parse_score_bound(low), parse_score_bound(high))
        else {
            return Err(CommandError::Failed("min or max is not a float".to_owned()));
        };
        let (with_scores, limit) = parse_score_range_options(&request.arguments, 4)?;
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::array(vec![])),
            Loaded::Found(entries) => entries,
        };
        sort_zset(&mut entries);
        let mut selected: Vec<(Vec<u8>, f64)> = entries.into_iter().filter(|(_, score)| in_score_range(*score, min, max)).collect();
        if reverse {
            selected.reverse();
        }
        if let Some((offset, count)) = limit {
            selected = apply_limit(&selected, offset, count);
        }
        return Ok(zset_entries_reply(&selected, with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZRANGEBYLEX") || command.eq_ignore_ascii_case(b"ZREVRANGEBYLEX") {
        let reverse = command.eq_ignore_ascii_case(b"ZREVRANGEBYLEX");
        if request.arguments.len() < 4 {
            let name = if reverse { "zrevrangebylex" } else { "zrangebylex" };
            return Err(arity_error(name));
        }
        let key = request.arguments[1];
        let (high, low) = if reverse { (request.arguments[2], request.arguments[3]) } else { (request.arguments[3], request.arguments[2]) };
        let (Some(min), Some(max)) = (parse_lex_bound(low), parse_lex_bound(high))
        else {
            return Err(CommandError::Failed("min or max not valid string range item".to_owned()));
        };
        let limit = parse_lex_options(&request.arguments, 4)?;
        let entries = match load_zset(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::array(vec![])),
            Loaded::Found(entries) => entries,
        };
        // 与真机一致：按 (score, member) 顺序定位整段，分数不一致时保留
        // 分数顺序而不是重排成字典序。
        let mut entries = entries;
        sort_zset(&mut entries);
        let mut members: Vec<Vec<u8>> = match lex_block(&entries, &min, &max, reverse) {
            Some((start, end)) => entries[start..=end].iter().map(|(member, _)| member.clone()).collect(),
            None => Vec::new(),
        };
        if reverse {
            members.reverse();
        }
        if let Some((offset, count)) = limit {
            members = apply_limit(&members, offset, count);
        }
        let items: Vec<Reply> = members.into_iter().map(Reply::bulk).collect();
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"ZRANK") || command.eq_ignore_ascii_case(b"ZREVRANK") {
        let reverse = command.eq_ignore_ascii_case(b"ZREVRANK");
        if request.arguments.len() != 3 && request.arguments.len() != 4 {
            let name = if reverse { "zrevrank" } else { "zrank" };
            return Err(arity_error(name));
        }
        let with_score = request.arguments.len() == 4;
        if with_score && !request.arguments[3].eq_ignore_ascii_case(b"WITHSCORE") {
            return Err(CommandError::Failed("syntax error".to_owned()));
        }
        let key = request.arguments[1];
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => {
                return if with_score { Ok(Reply::nil_array()) } else { Ok(Reply::nil()) };
            }
            Loaded::Found(entries) => entries,
        };
        sort_zset(&mut entries);
        return match entries.iter().position(|(member, _)| member.as_slice() == request.arguments[2]) {
            Some(index) => {
                let rank = if reverse { entries.len() - 1 - index } else { index } as i64;
                if with_score { Ok(Reply::array(vec![Reply::integer(rank), score_reply(entries[index].1)])) } else { Ok(Reply::integer(rank)) }
            }
            // `WITHSCORE` answers an array on success, so the server keeps the array
            // shape for a miss and replies a null array under RESP2.
            None => {
                if with_score {
                    Ok(Reply::nil_array())
                }
                else {
                    Ok(Reply::nil())
                }
            }
        };
    }
    if command.eq_ignore_ascii_case(b"ZINCRBY") {
        if request.arguments.len() != 4 {
            return Err(arity_error("zincrby"));
        }
        let Some(increment) = parse_score(request.arguments[2])
        else {
            return Err(CommandError::Failed("value is not a valid float".to_owned()));
        };
        let key = request.arguments[1];
        match find_kind(executor, base, key) {
            Ok(None) | Ok(Some(KeyKind::ZSet)) => {}
            Ok(Some(_)) => return Err(CommandError::WrongType),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        }
        let mut entries = match load_value(executor, &KeyKind::ZSet.namespace(base), key) {
            Ok(Some(bytes)) => match decode_zset(&bytes) {
                Some(entries) => entries,
                None => return Err(CommandError::Failed("stored zset value is corrupt".to_owned())),
            },
            Ok(None) => Vec::new(),
            Err(message) => return Err(CommandError::Failed(message.to_owned())),
        };
        let member = request.arguments[3].to_vec();
        let new_score = match entries.iter().find(|(existing, _)| *existing == member) {
            Some((_, score)) => *score + increment,
            None => increment,
        };
        if new_score.is_nan() {
            return Err(CommandError::Failed("resulting score is not a number (NaN)".to_owned()));
        }
        match entries.iter_mut().find(|(existing, _)| *existing == member) {
            Some(entry) => entry.1 = new_score,
            None => entries.push((member, new_score)),
        }
        let Some(bytes) = encode_zset(&entries)
        else {
            return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
        };
        if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        return Ok(score_reply(new_score));
    }
    if command.eq_ignore_ascii_case(b"ZCOUNT") {
        if request.arguments.len() != 4 {
            return Err(arity_error("zcount"));
        }
        let (Some(min), Some(max)) = (parse_score_bound(request.arguments[2]), parse_score_bound(request.arguments[3]))
        else {
            return Err(CommandError::Failed("min or max is not a float".to_owned()));
        };
        let key = request.arguments[1];
        return match load_zset(executor, base, key)? {
            Loaded::Missing => Ok(Reply::integer(0)),
            Loaded::Found(entries) => Ok(Reply::integer(entries.iter().filter(|(_, score)| in_score_range(*score, min, max)).count() as i64)),
        };
    }
    if command.eq_ignore_ascii_case(b"ZREMRANGEBYRANK")
        || command.eq_ignore_ascii_case(b"ZREMRANGEBYSCORE")
        || command.eq_ignore_ascii_case(b"ZREMRANGEBYLEX")
    {
        let by_score = command.eq_ignore_ascii_case(b"ZREMRANGEBYSCORE");
        let by_lex = command.eq_ignore_ascii_case(b"ZREMRANGEBYLEX");
        let name = if by_score {
            "zremrangebyscore"
        }
        else if by_lex {
            "zremrangebylex"
        }
        else {
            "zremrangebyrank"
        };
        if request.arguments.len() != 4 {
            return Err(arity_error(name));
        }
        let mut min = ScoreBound { value: f64::NEG_INFINITY, exclusive: false };
        let mut max = ScoreBound { value: f64::INFINITY, exclusive: false };
        let mut rank_range: Option<(i64, i64)> = None;
        let mut lex_range: Option<(LexBound, LexBound)> = None;
        if by_score {
            match (parse_score_bound(request.arguments[2]), parse_score_bound(request.arguments[3])) {
                (Some(low), Some(high)) => {
                    min = low;
                    max = high;
                }
                _ => return Err(CommandError::Failed("min or max is not a float".to_owned())),
            }
        }
        else if by_lex {
            match (parse_lex_bound(request.arguments[2]), parse_lex_bound(request.arguments[3])) {
                (Some(low), Some(high)) => lex_range = Some((low, high)),
                _ => return Err(CommandError::Failed("min or max not valid string range item".to_owned())),
            }
        }
        else {
            let (Some(low), Some(high)) = (parse_index(request.arguments[2]), parse_index(request.arguments[3]))
            else {
                return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
            };
            rank_range = Some((low, high));
        }
        let key = request.arguments[1];
        let mut removed = 0_i64;
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => return Ok(Reply::integer(0)),
            Loaded::Found(entries) => entries,
        };
        if by_score {
            entries.retain(|(_, score)| {
                let hit = in_score_range(*score, min, max);
                if hit {
                    removed += 1;
                }
                !hit
            });
        }
        else if let Some((low, high)) = lex_range {
            // 真机按 (score, member) 顺序定位首尾后整段删除。
            sort_zset(&mut entries);
            if let Some((start, end)) = lex_block(&entries, &low, &high, false) {
                entries.drain(start..=end);
                removed = (end - start + 1) as i64;
            }
        }
        else {
            sort_zset(&mut entries);
            if let Some((start, stop)) = rank_range {
                if let Some((begin, end)) = entry_rank_range(start, stop, entries.len()) {
                    entries.drain(begin..=end);
                    removed = (end - begin + 1) as i64;
                }
            }
        }
        if removed == 0 {
            return Ok(Reply::integer(0));
        }
        if entries.is_empty() {
            if let Err(message) = remove_value(executor, &KeyKind::ZSet.namespace(base), key) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        else {
            let Some(bytes) = encode_zset(&entries)
            else {
                return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
            };
            if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(removed));
    }
    if command.eq_ignore_ascii_case(b"ZMSCORE") {
        if request.arguments.len() < 3 {
            return Err(arity_error("zmscore"));
        }
        let entries = match load_zset(executor, base, request.arguments[1])? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(entries) => entries,
        };
        let items: Vec<Reply> = request.arguments[2..]
            .iter()
            .map(|member| match entries.iter().find(|(candidate, _)| candidate.as_slice() == *member) {
                Some((_, score)) => score_reply(*score),
                None => Reply::nil(),
            })
            .collect();
        return Ok(Reply::array(items));
    }
    if command.eq_ignore_ascii_case(b"ZLEXCOUNT") {
        if request.arguments.len() != 4 {
            return Err(arity_error("zlexcount"));
        }
        let (Some(min), Some(max)) = (parse_lex_bound(request.arguments[2]), parse_lex_bound(request.arguments[3]))
        else {
            return Err(CommandError::Failed("min or max not valid string range item".to_owned()));
        };
        return match load_zset(executor, base, request.arguments[1])? {
            Loaded::Missing => Ok(Reply::integer(0)),
            Loaded::Found(entries) => {
                Ok(Reply::integer(entries.iter().filter(|(member, _)| lex_at_least(member, &min) && lex_at_most(member, &max)).count() as i64))
            }
        };
    }
    if command.eq_ignore_ascii_case(b"ZPOPMIN") || command.eq_ignore_ascii_case(b"ZPOPMAX") {
        let maximum = command.eq_ignore_ascii_case(b"ZPOPMAX");
        let name = if maximum { "zpopmax" } else { "zpopmin" };
        if request.arguments.len() < 2 || request.arguments.len() > 3 {
            return Err(arity_error(name));
        }
        let with_count = request.arguments.len() == 3;
        let count = if with_count {
            match parse_index(request.arguments[2]) {
                Some(value) if value >= 0 => value as usize,
                // 真机对非整数与负数使用同一条「必须为正」的提示。
                _ => return Err(CommandError::Failed("value is out of range, must be positive".to_owned())),
            }
        }
        else {
            1
        };
        let key = request.arguments[1];
        let mut entries = match load_zset(executor, base, key)? {
            Loaded::Missing => {
                return Ok(Reply::array(vec![]));
            }
            Loaded::Found(entries) => entries,
        };
        sort_zset(&mut entries);
        if maximum {
            entries.reverse();
        }
        let take = count.min(entries.len());
        let popped = entries[..take].to_vec();
        if take > 0 {
            let remaining = entries[take..].to_vec();
            if remaining.is_empty() {
                if let Err(message) = remove_value(executor, &KeyKind::ZSet.namespace(base), key) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
            else {
                let Some(bytes) = encode_zset(&remaining)
                else {
                    return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
                };
                if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
        }
        if with_count {
            return Ok(zset_entries_reply(&popped, true, protocol));
        }
        if popped.is_empty() {
            return Ok(Reply::array(vec![]));
        }
        return Ok(Reply::array(vec![Reply::bulk(popped[0].0.clone()), score_reply(popped[0].1)]));
    }
    if command.eq_ignore_ascii_case(b"ZRANDMEMBER") {
        if request.arguments.len() < 2 || request.arguments.len() > 4 {
            return Err(arity_error("zrandmember"));
        }
        let entries = match load_zset(executor, base, request.arguments[1])? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(entries) => entries,
        };
        if entries.is_empty() {
            return if request.arguments.len() == 2 { Ok(Reply::nil()) } else { Ok(Reply::array(vec![])) };
        }
        if request.arguments.len() == 2 {
            let index = random_below(entries.len());
            return Ok(Reply::bulk(entries[index].0.to_vec()));
        }
        let with_scores = match request.arguments.len() {
            3 => false,
            4 if request.arguments[3].eq_ignore_ascii_case(b"WITHSCORES") => true,
            _ => return Err(CommandError::Failed("syntax error".to_owned())),
        };
        let Some(count) = parse_index(request.arguments[2])
        else {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        };
        if count == 0 {
            return Ok(Reply::array(vec![]));
        }
        let selected = if count > 0 {
            sample_distinct(&entries, count as usize)
        }
        else {
            let take = count.unsigned_abs() as usize;
            (0..take).map(|_| entries[random_below(entries.len())].clone()).collect()
        };
        return Ok(zset_entries_reply(&selected, with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZMPOP") {
        if request.arguments.len() < 4 {
            return Err(arity_error("zmpop"));
        }
        return handle_zset_mpop(executor, base, &request.arguments, 1, protocol);
    }
    if command.eq_ignore_ascii_case(b"BZMPOP") {
        if request.arguments.len() < 5 {
            return Err(arity_error("bzmpop"));
        }
        if let Some(protocol_error) = validate_timeout(&request.arguments[1]) {
            return Err(CommandError::Failed(protocol_error.to_owned()));
        }
        return handle_zset_mpop(executor, base, &request.arguments, 2, protocol);
    }
    if command.eq_ignore_ascii_case(b"BZPOPMIN") || command.eq_ignore_ascii_case(b"BZPOPMAX") {
        let maximum = command.eq_ignore_ascii_case(b"BZPOPMAX");
        let name = if maximum { "bzpopmax" } else { "bzpopmin" };
        if request.arguments.len() < 3 {
            return Err(arity_error(name));
        }
        if let Some(message) = validate_timeout(request.arguments[request.arguments.len() - 1]) {
            return Err(CommandError::Failed(message.to_owned()));
        }
        let keys = &request.arguments[1..request.arguments.len() - 1];
        for key in keys {
            let mut entries = match load_zset(executor, base, key)? {
                Loaded::Missing => continue,
                Loaded::Found(entries) => entries,
            };
            if entries.is_empty() {
                continue;
            }
            sort_zset(&mut entries);
            let take = if maximum { entries.len() - 1 } else { 0 };
            let (member, score) = entries.remove(take);
            if entries.is_empty() {
                if let Err(message) = remove_value(executor, &KeyKind::ZSet.namespace(base), key) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
            else {
                let Some(bytes) = encode_zset(&entries)
                else {
                    return Err(CommandError::Failed("zset exceeds the storage limit".to_owned()));
                };
                if let Err(message) = store_value(executor, &KeyKind::ZSet.namespace(base), key, bytes) {
                    return Err(CommandError::Failed(message.to_owned()));
                }
            }
            return Ok(Reply::array(vec![Reply::bulk(key.to_vec()), Reply::bulk(member), score_reply(score)]));
        }
        return Ok(Reply::nil_array());
    }
    if command.eq_ignore_ascii_case(b"ZUNION") || command.eq_ignore_ascii_case(b"ZINTER") {
        let union = command.eq_ignore_ascii_case(b"ZUNION");
        let name = if union { "zunion" } else { "zinter" };
        if request.arguments.len() < 3 {
            return Err(arity_error(name));
        }
        let (plan, with_scores) = parse_zset_combine_plan(name, &request.arguments, 1)?;
        let sets = load_zsets(executor, base, &plan.keys)?;
        let result = combine_zsets(&sets, &plan.weights, plan.aggregate, if union { ZSetOp::Union } else { ZSetOp::Inter });
        return Ok(zset_entries_reply(&result, with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZSCAN") {
        if request.arguments.len() < 3 {
            return Err(arity_error("zscan"));
        }
        let cursor = parse_scan_cursor(request.arguments[2])?;
        let (pattern, count) = parse_scan_tail(&request.arguments, 3)?;
        let key = request.arguments[1];
        let entries = match load_zset(executor, base, key)? {
            Loaded::Missing => Vec::new(),
            Loaded::Found(entries) => entries,
        };
        return Ok(zscan_page_reply(&entries, cursor, count, pattern.as_deref()));
    }
    if command.eq_ignore_ascii_case(b"ZRANGE") {
        if request.arguments.len() < 4 {
            return Err(arity_error("zrange"));
        }
        // 整段读是拉取大集合的常见形态：跳过逐成员解码与二次克隆。
        if is_full_zset_range(&request.arguments) {
            let with_scores = request.arguments.len() == 5;
            return read_full_zset(executor, base, request.arguments[1], with_scores, protocol);
        }
        let range = parse_range_request(&request.arguments, (2, 3), 4, true)?;
        let selected = select_range(executor, base, request.arguments[1], &range)?;
        return Ok(zset_entries_reply(&selected, range.with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZREVRANGE") {
        if request.arguments.len() < 4 {
            return Err(arity_error("zrevrange"));
        }
        let with_scores = match request.arguments.len() {
            4 => false,
            5 if request.arguments[4].eq_ignore_ascii_case(b"WITHSCORES") => true,
            _ => return Err(CommandError::Failed("syntax error".to_owned())),
        };
        let range = RangeRequest {
            mode: RangeMode::Rank,
            reverse: true,
            with_scores,
            limit: None,
            first: request.arguments[2].to_vec(),
            second: request.arguments[3].to_vec(),
        };
        let selected = select_range(executor, base, request.arguments[1], &range)?;
        return Ok(zset_entries_reply(&selected, with_scores, protocol));
    }
    if command.eq_ignore_ascii_case(b"ZRANGESTORE") {
        if request.arguments.len() < 5 {
            return Err(arity_error("zrangestore"));
        }
        let range = parse_range_request(&request.arguments, (3, 4), 5, false)?;
        let selected = select_range(executor, base, request.arguments[2], &range)?;
        return store_zset_result(executor, base, request.arguments[1], &selected);
    }
    Err(CommandError::Failed("unsupported command".to_owned()))
}

/// HyperLogLog 的精度参数 `P`，寄存器数量为 `1 << P`。
const HLL_P: u32 = 14;
/// 估计所需的 `Q = 64 - P`，寄存器值的上限是 `Q + 1`。
const HLL_Q: usize = 50;
/// 寄存器数量。
const HLL_REGISTERS: usize = 1 << HLL_P;
/// 每个寄存器占用的位数。
const HLL_BITS: usize = 6;
/// 单个寄存器能表示的最大值。
const HLL_REGISTER_MAX: u8 = 63;
/// dense 编码的 16 字节头长度。
const HLL_HEADER_SIZE: usize = 16;
/// 16384 个 6 位寄存器正好占 12288 字节。
const HLL_REGISTER_BYTES: usize = HLL_REGISTERS * HLL_BITS / 8;
/// dense 编码的完整长度。
const HLL_DENSE_SIZE: usize = HLL_HEADER_SIZE + HLL_REGISTER_BYTES;
/// 估计量里的 `alpha`，等同 `0.5 / ln(2)`。
const HLL_ALPHA_INF: f64 = 0.721347520444481703680;
/// 新写入后把缓存的基数标记为失效的位。
const HLL_CACHE_INVALID: u8 = 0x80;

/// 报出「不是合法 HyperLogLog」的类型错误。
///
/// 真机对普通字符串与非 dense 编码统一用这条文案，而不是常规的 `WRONGTYPE` 文案。
fn invalid_hll_error() -> CommandError {
    CommandError::Coded("WRONGTYPE Key is not a valid HyperLogLog string value.".to_owned())
}

/// 创建一个空的 dense HyperLogLog：`"HYLL"` 头、dense 编码、全零寄存器。
fn new_hll() -> Vec<u8> {
    let mut bytes = vec![0_u8; HLL_DENSE_SIZE];
    bytes[0..4].copy_from_slice(b"HYLL");
    bytes[4] = 0;
    bytes
}

/// 校验字符串内容是否是一个 dense HyperLogLog，并切出寄存器区。
///
/// 只接受与真机 dense 编码长度一致的对象；稀疏编码不生成也不接受。
fn hll_registers(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() != HLL_DENSE_SIZE || &bytes[0..4] != b"HYLL" || bytes[4] != 0 {
        return None;
    }
    Some(&bytes[HLL_HEADER_SIZE..])
}

/// 一次 HLL 读取的结果。
enum HllLoaded {
    /// 键不存在或已过期。
    Missing,
    /// 合法的 dense HLL 原始字节。
    Found(Vec<u8>),
}

/// 读取一个 HLL 键。
///
/// HLL 在真机上就是一个字符串，所以它落在字符串命名空间，`TYPE` 也回复 `string`。
fn load_hll(executor: &dyn KeyValueExecutor, base: &Namespace, key: &[u8]) -> Result<HllLoaded, CommandError> {
    match find_kind(executor, base, key) {
        Ok(None) => return Ok(HllLoaded::Missing),
        Ok(Some(KeyKind::String)) => {}
        Ok(Some(_)) => {
            return Err(CommandError::WrongType);
        }
        Err(message) => {
            return Err(CommandError::Failed(message.to_owned()));
        }
    }
    match load_value(executor, base, key) {
        Ok(Some(bytes)) => {
            if hll_registers(&bytes).is_none() {
                return Err(invalid_hll_error());
            }
            Ok(HllLoaded::Found(bytes))
        }
        Ok(None) => Ok(HllLoaded::Missing),
        Err(message) => Err(CommandError::Failed(message.to_owned())),
    }
}

/// Redis 使用的 MurmurHash64A，种子固定为 `0xadc83b19`。
fn murmur_hash64a(bytes: &[u8], seed: u64) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    const R: u32 = 47;
    let length = bytes.len();
    let mut hash = seed ^ (length as u64).wrapping_mul(M);
    let mut offset = 0;
    while offset + 8 <= length {
        let mut block = u64::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]);
        block = block.wrapping_mul(M);
        block ^= block >> R;
        block = block.wrapping_mul(M);
        hash ^= block;
        hash = hash.wrapping_mul(M);
        offset += 8;
    }
    let tail = &bytes[offset..];
    if !tail.is_empty() {
        let mut extra = 0_u64;
        for (index, byte) in tail.iter().enumerate() {
            extra |= (*byte as u64) << (8 * index);
        }
        hash ^= extra;
        hash = hash.wrapping_mul(M);
    }
    hash ^= hash >> R;
    hash = hash.wrapping_mul(M);
    hash ^= hash >> R;
    hash
}

/// 读取一个 dense 寄存器。
fn hll_get_register(registers: &[u8], index: usize) -> u8 {
    let bit = index * HLL_BITS;
    let byte = bit / 8;
    let shift = bit % 8;
    let low = registers[byte] as u16;
    let high = registers.get(byte + 1).copied().unwrap_or(0) as u16;
    (((low >> shift) | (high << (8 - shift)) & 0xffff) & HLL_REGISTER_MAX as u16) as u8
}

/// 写入一个 dense 寄存器。
fn hll_set_register(registers: &mut [u8], index: usize, value: u8) {
    let value = value & HLL_REGISTER_MAX;
    let bit = index * HLL_BITS;
    let byte = bit / 8;
    let shift = bit % 8;
    let low_bits = (8 - shift).min(HLL_BITS);
    let low_mask = (((1_u16 << low_bits) - 1) as u8) << shift;
    registers[byte] = (registers[byte] & !low_mask) | (((value as u16) << shift) as u8 & low_mask);
    if shift + HLL_BITS > 8 {
        let spill = shift + HLL_BITS - 8;
        let mask = (1_u8 << spill) - 1;
        if let Some(next) = registers.get_mut(byte + 1) {
            *next = (*next & !mask) | ((value >> (8 - shift)) & mask);
        }
    }
}

/// 把一个元素映射为「寄存器下标 + 尾零计数」。
fn hll_pat_len(element: &[u8]) -> (usize, u8) {
    let mut hash = murmur_hash64a(element, 0xadc83b19);
    let index = (hash & (HLL_REGISTERS as u64 - 1)) as usize;
    hash >>= HLL_P;
    hash |= 1_u64 << HLL_Q;
    let mut bit = 1_u64;
    let mut count = 1_u8;
    while hash & bit == 0 {
        count += 1;
        bit <<= 1;
    }
    (index, count)
}

/// Ertl 的 `tau` 修正项。
fn hll_tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let mut y = 1.0_f64;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let previous = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if previous == z {
            break;
        }
    }
    z / 3.0
}

/// Ertl 的 `sigma` 修正项。
fn hll_sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0_f64;
    let mut z = x;
    loop {
        x *= x;
        let previous = z;
        z += x * y;
        y += y;
        if previous == z {
            break;
        }
    }
    z
}

/// 从寄存器直方图做基数估计，公式与真机一致。
fn hll_estimate(registers: &[u8]) -> i64 {
    let m = HLL_REGISTERS as f64;
    let mut histogram = [0_u64; 64];
    for index in 0..HLL_REGISTERS {
        histogram[hll_get_register(registers, index) as usize] += 1;
    }
    let mut z = m * hll_tau((m - histogram[HLL_Q + 1] as f64) / m);
    for j in (1..=HLL_Q).rev() {
        z += histogram[j] as f64;
        z *= 0.5;
    }
    z += m * hll_sigma(histogram[0] as f64 / m);
    (HLL_ALPHA_INF * m * m / z).round() as i64
}

/// 把来源寄存器取最大值合并进目标寄存器。
fn hll_merge_max(registers: &mut [u8], source: &[u8]) {
    for index in 0..HLL_REGISTERS {
        let value = hll_get_register(source, index);
        if value > hll_get_register(registers, index) {
            hll_set_register(registers, index, value);
        }
    }
}

/// `PFADD` / `PFCOUNT` / `PFMERGE` 的实现。
///
/// 值本身是 dense HyperLogLog 字符串，创建时写入完整 12304 字节，读写只碰寄存器区。
pub fn handle_hyperloglog(request: &Request<'_>, executor: Option<&dyn KeyValueExecutor>, base: &Namespace) -> Result<Reply, CommandError> {
    let Some(executor) = executor
    else {
        return Err(CommandError::Failed("unsupported command".to_owned()));
    };
    let command = request.arguments[0];
    if command.eq_ignore_ascii_case(b"PFADD") {
        if request.arguments.len() < 2 {
            return Err(arity_error("pfadd"));
        }
        let key = request.arguments[1];
        let (mut object, created) = match load_hll(executor, base, key)? {
            HllLoaded::Missing => (new_hll(), true),
            HllLoaded::Found(bytes) => (bytes, false),
        };
        let mut modified = created;
        {
            let registers = &mut object[HLL_HEADER_SIZE..];
            for element in &request.arguments[2..] {
                let (index, count) = hll_pat_len(element);
                if hll_get_register(registers, index) < count {
                    hll_set_register(registers, index, count);
                    modified = true;
                }
            }
        }
        if modified {
            object[15] |= HLL_CACHE_INVALID;
            if let Err(message) = store_value(executor, base, key, object) {
                return Err(CommandError::Failed(message.to_owned()));
            }
        }
        return Ok(Reply::integer(i64::from(modified)));
    }
    if command.eq_ignore_ascii_case(b"PFCOUNT") {
        if request.arguments.len() < 2 {
            return Err(arity_error("pfcount"));
        }
        if request.arguments.len() == 2 {
            return match load_hll(executor, base, request.arguments[1])? {
                HllLoaded::Missing => Ok(Reply::integer(0)),
                HllLoaded::Found(bytes) => Ok(Reply::integer(hll_estimate(hll_registers(&bytes).unwrap()))),
            };
        }
        let mut merged = vec![0_u8; HLL_REGISTER_BYTES];
        for key in &request.arguments[1..] {
            match load_hll(executor, base, key)? {
                HllLoaded::Missing => {}
                HllLoaded::Found(bytes) => hll_merge_max(&mut merged, hll_registers(&bytes).unwrap()),
            }
        }
        return Ok(Reply::integer(hll_estimate(&merged)));
    }
    // `PFMERGE` 只给目标键、不带任何源键也是合法调用，会把目标折叠为空 HLL。
    if request.arguments.len() < 2 {
        return Err(arity_error("pfmerge"));
    }
    let mut merged = vec![0_u8; HLL_REGISTER_BYTES];
    for key in &request.arguments[1..] {
        match load_hll(executor, base, key)? {
            HllLoaded::Missing => {}
            HllLoaded::Found(bytes) => hll_merge_max(&mut merged, hll_registers(&bytes).unwrap()),
        }
    }
    let destination = request.arguments[1];
    let mut object = match load_hll(executor, base, destination)? {
        HllLoaded::Missing => new_hll(),
        HllLoaded::Found(bytes) => bytes,
    };
    object[HLL_HEADER_SIZE..].copy_from_slice(&merged);
    object[15] |= HLL_CACHE_INVALID;
    if let Err(message) = store_value(executor, base, destination, object) {
        return Err(CommandError::Failed(message.to_owned()));
    }
    Ok(Reply::ok())
}
