//! 重试写（retryable writes）的登记与应答重放。
//!
//! `mongod` 以 `(lsid, txnNumber, stmtId)` 为键把已落定的写应答存进
//! `config.transactions`，同一个重试请求直接回放原应答而不再执行一次。登记表
//! 的键推导、年龄与条数上限、回放时的 `retriedStmtIds` 注入都在本模块，宿主
//! 只负责在命令路由前后调用它。

use database_protocol_mongo::bson::{BsonDocument, BsonValue};

/// 可以从登记表回放的写命令。
const RETRYABLE_WRITE_COMMANDS: &[&str] = &["insert", "update", "delete", "findAndModify", "bulkWrite"];

/// 登记表的条数上限；超出后按最旧条目先淘汰。
pub const RETRY_MAX_ENTRIES: usize = 4096;

/// 登记条目的存活上限（毫秒）。
pub const RETRY_MAX_AGE_MS: i64 = 30 * 60 * 1000;

/// `true` 表示该命令可以从登记表回放，而不是再执行一次。
pub fn retryable_write_command(name: &str) -> bool {
    RETRYABLE_WRITE_COMMANDS.contains(&name)
}

/// 一次重试写的身份：登记键文本与实际语句号。
///
/// 缺少 `stmtId` 时按 `0` 处理，这正是 `mongod` 为单语句写存储的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryKey {
    key: String,
    stmt_id: i64,
}

impl RetryKey {
    /// 登记表使用的键文本。
    pub fn text(&self) -> &str {
        &self.key
    }

    /// `stmtId`；缺失时为 `0`。
    pub fn stmt_id(&self) -> i64 {
        self.stmt_id
    }
}

/// 把 BSON 数值读成整数；非数值返回 `None`。
fn integer_value(value: &BsonValue) -> Option<i64> {
    match value {
        BsonValue::Int32(number) => Some(i64::from(*number)),
        BsonValue::Int64(number) => Some(*number),
        BsonValue::Double(number) => Some(*number as i64),
        _ => None,
    }
}

/// 命令携带的 `lsid.id` 二进制值。
fn session_id(command: &BsonDocument) -> Option<&database_protocol_mongo::bson::BinaryData> {
    let Some(BsonValue::Document(session)) = command.get("lsid")
    else {
        return None;
    };
    match session.get("id") {
        Some(BsonValue::Binary(id)) => Some(id),
        _ => None,
    }
}

/// 一条重试写命令的登记键；不可重试或没有 `txnNumber` 时返回 `None`。
pub fn retry_key(name: &str, command: &BsonDocument) -> Option<RetryKey> {
    if !retryable_write_command(name) {
        return None;
    }
    let session = session_id(command)?;
    let txn_number = command.get("txnNumber").and_then(integer_value)?;
    let stmt_id = command.get("stmtId").and_then(integer_value).unwrap_or(0);
    let subtype = match session.subtype {
        4 => "04".to_string(),
        other => format!("{other:02x}"),
    };
    let key = format!("{subtype}|{}|{txn_number}|{stmt_id}", database_protocol_mongo::json::base64_encode(&session.bytes));
    Some(RetryKey { key, stmt_id })
}

/// 一条已落定的写应答。
#[derive(Debug, Clone)]
struct RetryRecord {
    reply: BsonDocument,
    stored_at: i64,
}

/// 已落定写应答的登记表。
///
/// 按插入顺序保存，超过 [`RETRY_MAX_ENTRIES`] 条先淘汰最旧条目，超过
/// [`RETRY_MAX_AGE_MS`] 的条目在读写时清除。`mongod` 把同样的记录持久化在
/// `config.transactions`；重启会丢掉这里的内存记录。
#[derive(Debug, Default)]
pub struct RetryRegistry {
    records: Vec<(String, RetryRecord)>,
}

impl RetryRegistry {
    /// 空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` 表示登记表里没有条目。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 取一条未过期的已落定应答。
    pub fn get(&mut self, key: &str, now_millis: i64) -> Option<BsonDocument> {
        let index = self.records.iter().position(|(entry, _)| entry == key)?;
        if now_millis - self.records[index].1.stored_at > RETRY_MAX_AGE_MS {
            self.records.remove(index);
            return None;
        }
        Some(self.records[index].1.reply.clone())
    }

    /// 记下一次成功的写应答，先清过期条目再按条数上限淘汰。
    pub fn store(&mut self, key: &str, reply: &BsonDocument, now_millis: i64) {
        self.records.retain(|(_, record)| now_millis - record.stored_at <= RETRY_MAX_AGE_MS);
        while self.records.len() >= RETRY_MAX_ENTRIES {
            self.records.remove(0);
        }
        let record = RetryRecord { reply: reply.clone(), stored_at: now_millis };
        match self.records.iter_mut().find(|(entry, _)| entry == key) {
            Some(slot) => slot.1 = record,
            None => self.records.push((key.to_string(), record)),
        }
    }
}

/// 回放一条登记应答：在 `ok` 之前插入 `retriedStmtIds`。
///
/// `mongod 8.0.9` 把该字段放在 `ok` 紧前面，`opTime` 与 `operationTime` 沿用
/// 缓存值，只有 `$clusterTime` 在重新发送时重签。
pub fn replayed_reply(stored: &BsonDocument, stmt_id: i64) -> BsonDocument {
    let mut result = BsonDocument::new();
    for (key, value) in stored.fields() {
        if key == "ok" {
            result.push("retriedStmtIds".to_string(), BsonValue::Array(vec![BsonValue::Int32(stmt_id as i32)]));
        }
        result.push(key.clone(), value.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use database_protocol_mongo::bson::BinaryData;

    use super::*;

    fn write_command(name: &str, txn_number: Option<i64>, stmt_id: Option<i64>) -> BsonDocument {
        let mut session = BsonDocument::new();
        session.push("id".to_string(), BsonValue::Binary(BinaryData::new(4, vec![1, 2, 3, 4])));
        let mut command = BsonDocument::new();
        command.push(name.to_string(), BsonValue::String("c".to_string()));
        command.push("lsid".to_string(), BsonValue::Document(session));
        if let Some(number) = txn_number {
            command.push("txnNumber".to_string(), BsonValue::Int64(number));
        }
        if let Some(number) = stmt_id {
            command.push("stmtId".to_string(), BsonValue::Int32(number as i32));
        }
        command
    }

    #[test]
    fn retry_key_carries_lsid_txn_and_statement() {
        let key = retry_key("insert", &write_command("insert", Some(7), Some(2))).expect("retryable");
        assert_eq!(key.text(), "04|AQIDBA==|7|2");
        assert_eq!(key.stmt_id(), 2);
        let defaulted = retry_key("update", &write_command("update", Some(7), None)).expect("retryable");
        assert_eq!(defaulted.text(), "04|AQIDBA==|7|0");
        assert!(retry_key("insert", &write_command("insert", None, Some(1))).is_none());
        assert!(retry_key("find", &write_command("find", Some(7), Some(1))).is_none());
    }

    #[test]
    fn registry_drops_expired_entries() {
        let mut registry = RetryRegistry::new();
        let mut reply = BsonDocument::new();
        reply.push("n".to_string(), BsonValue::Int32(1));
        registry.store("a", &reply, 1_000);
        assert_eq!(registry.len(), 1);
        assert!(registry.get("a", 1_000 + RETRY_MAX_AGE_MS).is_some());
        assert!(registry.get("a", 1_001 + RETRY_MAX_AGE_MS).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_evicts_the_oldest_entry_at_capacity() {
        let mut registry = RetryRegistry::new();
        let mut reply = BsonDocument::new();
        reply.push("ok".to_string(), BsonValue::Double(1.0));
        for index in 0..RETRY_MAX_ENTRIES {
            registry.store(&format!("key-{index}"), &reply, 0);
        }
        assert_eq!(registry.len(), RETRY_MAX_ENTRIES);
        registry.store("overflow", &reply, 0);
        assert_eq!(registry.len(), RETRY_MAX_ENTRIES);
        assert!(registry.get("key-0", 0).is_none());
        assert!(registry.get("overflow", 0).is_some());
    }

    #[test]
    fn replay_puts_retried_statement_ids_before_ok() {
        let mut stored = BsonDocument::new();
        stored.push("n".to_string(), BsonValue::Int32(3));
        stored.push("ok".to_string(), BsonValue::Double(1.0));
        let replay = replayed_reply(&stored, 4);
        let keys: Vec<&str> = replay.fields().iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, vec!["n", "retriedStmtIds", "ok"]);
        assert_eq!(replay.get("retriedStmtIds"), Some(&BsonValue::Array(vec![BsonValue::Int32(4)])));
    }
}
