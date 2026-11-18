//! `writeConcern` 校验、`$readPreference` 校验与写应答装饰。
//!
//! 判定结果只用 BSON 值表达，宿主拿到的是可直接写回线的应答片段，不再自己
//! 复制一套 `mongod` 的错误码、文案与字段顺序。规则来自 `mongod 8.0.9`
//! 的实测：单机拒绝 `w > 1` 与非 `majority` 的名字模式，单成员副本集把
//! `w > 1` 判为不可满足（`100`），未知名字模式答 `79` 但写入照常成功。

use database_protocol_mongo::bson::{BsonDocument, BsonValue};

use crate::mongo::commands::CommandError;

/// 应答通过 `writeConcern` 确认的命令。
const WRITE_CONCERN_COMMANDS: &[&str] = &[
    "insert",
    "update",
    "delete",
    "findAndModify",
    "bulkWrite",
    "create",
    "drop",
    "collMod",
    "createIndexes",
    "dropIndexes",
    "dropDatabase",
    "createUser",
    "dropUser",
    "commitTransaction",
];

/// 应答里带 `n` / `writeErrors` 因而可以被“不确认”的命令。
const COUNTER_COMMANDS: &[&str] = &["insert", "update", "delete"];

/// 副本集应答里带 `electionId` 与 `opTime` 的命令。
const STAMPED_COMMANDS: &[&str] = &["insert", "update", "delete"];

/// `$readPreference.mode` 的合法枚举值。
const READ_PREFERENCE_MODES: &[&str] = &["primary", "primaryPreferred", "secondary", "secondaryPreferred", "nearest"];

/// `true` 表示该命令的应答经 `writeConcern` 确认。
pub fn is_write_concern_command(name: &str) -> bool {
    WRITE_CONCERN_COMMANDS.contains(&name)
}

/// 一次命令的 `writeConcern` 判定。
#[derive(Debug, Clone, Default)]
pub struct WriteConcernDecision {
    /// `w: 0`：写入照常执行，但计数不确认。
    pub unacknowledged: bool,
    /// 直接替代命令执行的拒绝应答（写入没有发生）。
    pub error: Option<CommandError>,
    /// 成功应答上附加的 `writeConcernError`。
    pub write_concern_error: Option<BsonValue>,
}

/// BSON 标量在 `mongod` 类型错误里的名字。
pub fn bson_type_name(value: &BsonValue) -> &'static str {
    match value {
        BsonValue::Null => "null",
        BsonValue::String(_) => "string",
        BsonValue::Bool(_) => "bool",
        BsonValue::Int32(_) | BsonValue::Int64(_) => "int",
        BsonValue::Double(_) => "double",
        BsonValue::Decimal128(_) => "decimal",
        BsonValue::Array(_) => "array",
        BsonValue::ObjectId(_) => "objectId",
        BsonValue::DateTime(_) => "date",
        BsonValue::Timestamp { .. } => "timestamp",
        BsonValue::Binary(_) => "binData",
        BsonValue::Regex { .. } => "regex",
        BsonValue::MinKey => "minKey",
        BsonValue::MaxKey => "maxKey",
        BsonValue::Document(_) => "object",
        _ => "object",
    }
}

/// 把 BSON 数值解成 `f64`，非数值返回 `None`。
fn unwrap_number(value: &BsonValue) -> Option<f64> {
    match value {
        BsonValue::Int32(number) => Some(f64::from(*number)),
        BsonValue::Int64(number) => Some(*number as f64),
        BsonValue::Double(number) if number.is_finite() => Some(*number),
        BsonValue::Decimal128(number) => number.as_str().parse::<f64>().ok().filter(|parsed| parsed.is_finite()),
        _ => None,
    }
}

/// 把 BSON 标量渲染成错误文案里的文本。
fn scalar_text(value: &BsonValue) -> String {
    match value {
        BsonValue::Null => "null".to_string(),
        BsonValue::String(text) => text.clone(),
        BsonValue::Bool(flag) => flag.to_string(),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => format!("{number}"),
        BsonValue::Decimal128(number) => number.as_str().to_string(),
        _ => "[object]".to_string(),
    }
}

/// 构造 `writeConcernError`：`wtimeout` 与 `w` 原样回显，`provenance` 标明来源。
fn concern_error(code: i32, code_name: &str, message: &str, w: &BsonValue, wtimeout: f64) -> BsonValue {
    let mut concern = BsonDocument::new();
    concern.push("w".to_string(), w.clone());
    concern.push("wtimeout".to_string(), number_value(wtimeout));
    concern.push("provenance".to_string(), BsonValue::String("clientSupplied".to_string()));
    let mut error_info = BsonDocument::new();
    error_info.push("writeConcern".to_string(), BsonValue::Document(concern));
    let mut document = BsonDocument::new();
    document.push("code".to_string(), BsonValue::Int32(code));
    document.push("codeName".to_string(), BsonValue::String(code_name.to_string()));
    document.push("errmsg".to_string(), BsonValue::String(message.to_string()));
    document.push("errInfo".to_string(), BsonValue::Document(error_info));
    BsonValue::Document(document)
}

/// 回显数值：整数值用 `Int32`，其余用 `Double`，与解码后再编码的结果一致。
fn number_value(number: f64) -> BsonValue {
    if number.fract() == 0.0 && number >= f64::from(i32::MIN) && number <= f64::from(i32::MAX) {
        BsonValue::Int32(number as i32)
    }
    else {
        BsonValue::Double(number)
    }
}

/// 应用 `mongod` 的 `writeConcern` 校验。
///
/// `replica_set_name` 为 `None` 表示单机。数值 `w` 超过 50 或为负在任何拓扑都
/// 被拒绝；单机上数值 `w > 1` 与非 `majority` 的名字模式被拒绝；单成员副本集
/// 上数值 `w > 1` 判为不可满足（`100`），未知名字模式答 `79` 但写入仍执行。
pub fn evaluate_write_concern(command: &BsonDocument, replica_set_name: Option<&str>) -> WriteConcernDecision {
    let Some(raw) = command.get("writeConcern") else {
        return WriteConcernDecision::default();
    };
    let BsonValue::Document(raw) = raw else {
        return WriteConcernDecision {
            error: Some(CommandError::new(
                14,
                format!("\"writeConcern\" had the wrong type. Expected object, found {}", bson_type_name(raw)),
            )),
            ..WriteConcernDecision::default()
        };
    };

    let wtimeout = raw.get("wtimeout").and_then(unwrap_number).unwrap_or(0.0);
    let raw_w = raw.get("w");
    let w = raw_w.map_or(Some(1.0), unwrap_number);

    let Some(w) = w else {
        let raw_w = raw_w.expect("a missing `w` defaults to one");
        if let BsonValue::String(name) = raw_w {
            // 名字模式只在副本集上有意义。
            if replica_set_name.is_none() {
                if name != "majority" {
                    return WriteConcernDecision {
                        error: Some(CommandError::new(
                            2,
                            format!("cannot use non-majority 'w' mode \"{name}\" when a host is not a member of a replica set"),
                        )),
                        ..WriteConcernDecision::default()
                    };
                }
                return WriteConcernDecision::default();
            }
            if name == "majority" {
                return WriteConcernDecision::default();
            }
            return WriteConcernDecision {
                write_concern_error: Some(concern_error(
                    79,
                    "UnknownReplWriteConcern",
                    &format!("No write concern mode named '{name}' found in replica set configuration"),
                    raw_w,
                    wtimeout,
                )),
                ..WriteConcernDecision::default()
            };
        }
        if matches!(raw_w, BsonValue::Document(_)) {
            return WriteConcernDecision::default();
        }
        return WriteConcernDecision {
            error: Some(CommandError::new(
                9,
                format!("w has to be a number, string, or object; found: {}", bson_type_name(raw_w)),
            )),
            ..WriteConcernDecision::default()
        };
    };

    if !(0.0..=50.0).contains(&w) {
        return WriteConcernDecision {
            error: Some(CommandError::new(
                9,
                format!("w has to be a non-negative number and not greater than 50; found: {}", scalar_text_number(w)),
            )),
            ..WriteConcernDecision::default()
        };
    }
    if replica_set_name.is_none() && w > 1.0 {
        return WriteConcernDecision {
            error: Some(CommandError::new(2, "cannot use 'w' > 1 when a host is not replicated")),
            ..WriteConcernDecision::default()
        };
    }
    if w == 0.0 {
        return WriteConcernDecision { unacknowledged: true, ..WriteConcernDecision::default() };
    }
    if w > 1.0 {
        // 单成员集合永远满足不了 `w > 1`，`mongod` 立即答不可满足，即使带了
        // `wtimeout`。
        return WriteConcernDecision {
            write_concern_error: Some(concern_error(
                100,
                "UnsatisfiableWriteConcern",
                "Not enough data-bearing nodes",
                raw_w.expect("a numeric `w` is present"),
                wtimeout,
            )),
            ..WriteConcernDecision::default()
        };
    }
    WriteConcernDecision::default()
}

/// 数字回显：整数值不带小数点，与 JavaScript 的 `String(number)` 一致。
fn scalar_text_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e21 {
        format!("{}", number as i64)
    }
    else {
        format!("{number}")
    }
}

/// `$readPreference` 只做校验，路由仍是客户端的事；非法值返回错误。
pub fn read_preference_error(command: &BsonDocument) -> Option<CommandError> {
    let Some(raw) = command.get("$readPreference") else {
        return None;
    };
    let BsonValue::Document(raw) = raw else {
        return Some(CommandError::new(
            14,
            format!("$readPreference has incorrect type: expected object but got {}", bson_type_name(raw)),
        ));
    };
    let Some(mode) = raw.get("mode") else {
        return Some(CommandError::new(40414, "BSON field 'readPreference.mode' is missing but a required field"));
    };
    match mode {
        BsonValue::String(name) if READ_PREFERENCE_MODES.contains(&name.as_str()) => None,
        BsonValue::String(name) => Some(CommandError::new(
            2,
            format!("Enumeration value '{name}' for field 'readPreference.mode' is not a valid value."),
        )),
        other => Some(CommandError::new(
            2,
            format!("Enumeration value '{}' for field 'readPreference.mode' is not a valid value.", scalar_text(other)),
        )),
    }
}

/// 按 `mongod` 的字段顺序重写成功的写应答：
/// `n`、`electionId`、`opTime`、其余计数、`writeConcernError`、`ok`。
///
/// 不确认应答（`w: 0`）把 `n` / `nModified` 归零并丢掉错误列表，真机也是这样。
pub fn write_concern_reply(
    name: &str,
    body: &BsonDocument,
    decision: Option<&WriteConcernDecision>,
    stamps: Option<&BsonDocument>,
    ok: bool,
) -> BsonDocument {
    if !ok {
        return body.clone();
    }
    let unacknowledged = decision.is_some_and(|decision| decision.unacknowledged) && COUNTER_COMMANDS.contains(&name);
    let mut result = BsonDocument::new();
    for (key, value) in body.fields() {
        if key == "ok" || key == "writeConcernError" {
            continue;
        }
        if unacknowledged {
            if matches!(key.as_str(), "writeErrors" | "upserted" | "electionId" | "opTime") {
                continue;
            }
            if key == "n" || key == "nModified" {
                result.push(key.clone(), BsonValue::Int32(0));
                continue;
            }
        }
        result.push(key.clone(), value.clone());
        if key == "n" && !unacknowledged && stamps.is_some() && STAMPED_COMMANDS.contains(&name) {
            let stamps = stamps.expect("checked above");
            if let Some(election_id) = stamps.get("electionId") {
                result.push("electionId".to_string(), election_id.clone());
            }
            if let Some(op_time) = stamps.get("opTime") {
                result.push("opTime".to_string(), op_time.clone());
            }
        }
    }
    if !unacknowledged {
        if let Some(error) = decision.and_then(|decision| decision.write_concern_error.as_ref()) {
            result.push("writeConcernError".to_string(), error.clone());
        }
    }
    if let Some(ok) = body.get("ok") {
        result.push("ok".to_string(), ok.clone());
    }
    result
}
