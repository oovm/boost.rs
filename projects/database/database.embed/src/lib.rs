//! YYDB 进程内门面：`EmbedDb`。
//!
//! 远端形态（`database-client` 的 `yy::Client`）走 `yydb serve` 的 TCP 线协议；
//! 本 crate 面向**同进程宿主**（桌面壳的 runtime、测试、CLI），直接复用
//! `database-execution` 的 [`Connection`]，因此 schema、查询语义与 `.yydx`
//! 落盘布局与远端完全一致，不需要第二套存储实现。
//!
//! 宿主只通过 [`EmbedDb`] 拿 JSON 进出：参数是 `serde_json::Map`，查询结果是
//! 行对象数组。带宽度/时标语义的值沿用 `database-types` 的 tagged JSON 形态
//! （`$database` 成员），避免 64 位整数与时间在 JSON 边界被降精度。

#![deny(missing_docs)]

use std::{collections::BTreeMap, path::Path};

use database_execution::engine::{Connection, QueryRow};
use database_types::{Error as DbError, Value, scalar};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

/// 嵌入库失败分类。
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// 底层引擎错误（schema、约束、IO、事务）。
    #[error("database error: {0}")]
    Database(#[from] DbError),
    /// 参数里的 JSON 形状超出了 VOS 绑定支持的范围。
    #[error("unsupported json value: {0}")]
    UnsupportedJson(String),
}

/// 嵌入库结果别名。
pub type Result<T = ()> = std::result::Result<T, EmbedError>;

/// 进程内 YYDB 句柄。
///
/// 一个 [`EmbedDb`] 对应一个应用自己的 `.yydx` 主文件；宿主按应用隔离实例，
/// 不要把多个应用的记录写进同一个库。
pub struct EmbedDb {
    conn: Connection,
}

impl std::fmt::Debug for EmbedDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedDb").finish_non_exhaustive()
    }
}

impl EmbedDb {
    /// 打开（或创建）`path` 指向的 `.yydx` 主文件。
    ///
    /// 扩展名必须是 `yydx`：大对象随同落在同级 `<stem>-objects/` 目录。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_yydx(path)?;
        Ok(Self { conn })
    }

    /// 打开一个私有内存库（测试与一次性计算用，进程退出即丢）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    /// 确保 schema 与 `document`（VOS 文档）一致；重复调用必须幂等。
    pub fn ensure_schema(&self, document: &str) -> Result<()> {
        self.conn.ensure_schema(document)?;
        Ok(())
    }

    /// 当前 schema 修订号；空库返回 `None`。
    pub fn schema_version(&self) -> Result<Option<u32>> {
        Ok(self.conn.schema_version()?)
    }

    /// 执行一条只读 VOS 查询，返回行对象数组。
    pub fn query(&self, source: &str, params: &JsonMap<String, JsonValue>) -> Result<Vec<JsonValue>> {
        let named = bind(params)?;
        let rows: Vec<QueryRow> = self.conn.query_vos_with_params(source, &named)?;
        Ok(rows.iter().map(row_to_json).collect())
    }

    /// 执行一条 VOS 写语句（`insert` / `update` / `delete` / 迁移脚本）。
    pub fn execute(&self, source: &str, params: &JsonMap<String, JsonValue>) -> Result<()> {
        let named = bind(params)?;
        self.conn.execute_vos_write_with_params(source, &named)?;
        Ok(())
    }

    /// 写入一个原始 KV 记录（不经 schema；供宿主内部状态使用）。
    pub fn put_raw(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.conn.put(key, bytes)?;
        Ok(())
    }

    /// 读取一个原始 KV 记录。
    pub fn get_raw(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.conn.get(key)?)
    }
}

fn bind(params: &JsonMap<String, JsonValue>) -> Result<BTreeMap<String, Value>> {
    let mut named = BTreeMap::new();
    for (key, value) in params {
        named.insert(key.clone(), json_to_value(value)?);
    }
    Ok(named)
}

/// JSON → VOS 绑定值。
///
/// 覆盖标量、字节（tagged `bytes`）、UUID/时标（tagged 形态）与嵌套行；
/// 数组与对象不是 VOS 绑定接受的标量形状，显式报错而不是静默丢字段。
pub fn json_to_value(json: &JsonValue) -> Result<Value> {
    if let Some(tagged) = scalar::tagged_value_from_json(json) {
        return tagged.map_err(|error| EmbedError::UnsupportedJson(error.to_string()));
    }
    match json {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(inner) => Ok(Value::Bool(*inner)),
        JsonValue::Number(inner) => {
            if let Some(int) = inner.as_i64() {
                return Ok(Value::I64(int));
            }
            if let Some(uint) = inner.as_u64() {
                return Ok(Value::U64(uint));
            }
            inner.as_f64().map(Value::F64).ok_or_else(|| EmbedError::UnsupportedJson(inner.to_string()))
        }
        JsonValue::String(inner) => Ok(Value::Text(inner.clone())),
        JsonValue::Object(inner) if inner.contains_key(scalar::TAGGED_JSON_KEY) => {
            Err(EmbedError::UnsupportedJson(format!("unknown tagged scalar: {inner:?}")))
        }
        JsonValue::Object(inner) => {
            let mut row = BTreeMap::new();
            for (key, value) in inner {
                row.insert(key.clone(), json_to_value(value)?);
            }
            Ok(Value::Row(row))
        }
        JsonValue::Array(_) => Err(EmbedError::UnsupportedJson("array is not a bindable scalar".into())),
    }
}

/// VOS 查询行 → JSON 行对象。
///
/// 行内的 UUID/时标/精确整数沿用 tagged 形态，宿主可以原样回传给下一次查询。
pub fn row_to_json(row: &QueryRow) -> JsonValue {
    let mut object = JsonMap::new();
    for (key, value) in row {
        object.insert(key.clone(), value_to_json(value));
    }
    JsonValue::Object(object)
}

/// 单个值 → JSON。
pub fn value_to_json(value: &Value) -> JsonValue {
    if let Some(tagged) = scalar::tagged_value_json(value) {
        return tagged;
    }
    match value {
        Value::Null => JsonValue::Null,
        Value::Bool(inner) => JsonValue::Bool(*inner),
        Value::I64(inner) => json!(inner),
        Value::U64(inner) => json!(inner),
        Value::F64(inner) => json!(inner),
        Value::Text(inner) => JsonValue::String(inner.clone()),
        Value::Row(inner) => {
            let mut object = JsonMap::new();
            for (key, value) in inner {
                object.insert(key.clone(), value_to_json(value));
            }
            JsonValue::Object(object)
        }
        // 向量 / 对象 / 文件不参与应用数据面；保底给出 kind 标签，避免 panic。
        other => json!({ "$database": "unsupported", "kind": other.kind() }),
    }
}
