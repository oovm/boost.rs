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
//!
//! # 一个路径只允许一个持有者
//!
//! 文件库是单写者：一个 `.yydx` 句柄持有排他锁，直到最后一个句柄释放。所以
//! **同一进程里第二次打开同一路径会失败**，错误码是 `database.busy`
//! （[`EmbedError::Database`] 包装的 [`database_types::Error::Busy`]），可以用
//! `Error::is_busy` 判定并按退避重试。
//!
//! 一个宿主里有多个组件都要读写同一个库时，用 [`EmbedDb::open_shared`]：它按
//! 规范化路径复用同一个 [`Arc<EmbedDb>`]，所有组件共享一个连接，不会再撞锁。
//! 直接重复调用 [`EmbedDb::open`] 只会拿到 `database.busy`。
//!
//! 还需要注意：句柄存活期间锁一直被持有。**不要在持有 `AccountStore` 之类的
//! 句柄时跨网络调用**，也不要让两个 helper 各自在内部开库而外层还握着句柄——
//! 这正是共享入口要解决的问题。

#![warn(missing_docs)]

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

use database_execution::engine::{Connection, QueryRow, catalog::RenameMap};
use database_types::{Error as DbError, Value, scalar};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

/// One live handle registered for a canonical database path.
///
/// `live` keeps the entry observable for exactly as long as its owner holds the
/// handle: the owner owns a strong `Arc`, the registry only a `Weak`. Entries
/// whose strong count dropped are pruned on the next open, so a closed database
/// can be reopened without any explicit deregistration.
struct OpenEntry {
    live: Weak<()>,
    /// Present when the handle was created through [`EmbedDb::open_shared`].
    shared: Option<Weak<EmbedDb>>,
}

/// Process-wide ownership table keyed by canonical path.
static OPEN: LazyLock<Mutex<HashMap<PathBuf, OpenEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Canonical key for an open path, tolerating a file that does not exist yet.
fn open_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    match parent.canonicalize() {
        Ok(parent) => path.file_name().map(|name| parent.join(name)).unwrap_or(parent),
        Err(_) => path.to_path_buf(),
    }
}

/// Drop registry entries whose owner released the handle.
fn prune_open(map: &mut HashMap<PathBuf, OpenEntry>) {
    map.retain(|_, entry| entry.live.strong_count() > 0);
}

/// Build the contention error a second open reports.
fn already_open(key: &Path) -> EmbedError {
    EmbedError::Database(DbError::Busy { path: key.display().to_string(), mode: "writer" })
}

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
    /// Keeps this handle's registry entry observable for its whole lifetime.
    _live: Arc<()>,
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
    ///
    /// 一个进程里同一路径只能有一个持有者。路径已经被打开时返回
    /// `database.busy`，而不是让第二次 `open` 撞上底层的排他锁；需要用同一个库
    /// 的多个组件请改用 [`Self::open_shared`]。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let key = open_key(path);
        let mut map = OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_open(&mut map);
        if map.contains_key(&key) {
            return Err(already_open(&key));
        }
        // Reserve the key before creating the connection so a racing open in
        // another thread sees a live entry instead of competing for the lock.
        let live = Arc::new(());
        map.insert(key.clone(), OpenEntry { live: Arc::downgrade(&live), shared: None });
        match Connection::open_yydx(path) {
            Ok(conn) => Ok(Self { conn, _live: live }),
            Err(error) => {
                map.remove(&key);
                Err(EmbedError::Database(error))
            }
        }
    }

    /// 打开（或复用）`path` 的进程内共享句柄。
    ///
    /// 同一个规范化路径第一次调用会创建连接，之后返回同一个 [`Arc`]；宿主各组件
    /// 因此共用一份连接与一把锁，不会再出现「A 持句柄、B 再开一次」的
    /// `database.busy`。最后一个 [`Arc`] 释放时连接关闭，下一次调用重新打开。
    ///
    /// 路径已经被 [`Self::open`] 独占时返回 `database.busy`。
    pub fn open_shared(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref();
        let key = open_key(path);
        let mut map = OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_open(&mut map);
        if let Some(entry) = map.get(&key) {
            if let Some(shared) = entry.shared.as_ref().and_then(Weak::upgrade) {
                return Ok(shared);
            }
            return Err(already_open(&key));
        }
        let conn = Connection::open_yydx(path)?;
        let live = Arc::new(());
        let db = Arc::new(Self { conn, _live: live.clone() });
        map.insert(key, OpenEntry { live: Arc::downgrade(&live), shared: Some(Arc::downgrade(&db)) });
        Ok(db)
    }

    /// 打开一个私有内存库（测试与一次性计算用，进程退出即丢）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn, _live: Arc::new(()) })
    }

    /// 确保 schema 与 `document`（VOS 文档）一致；重复调用必须幂等。
    pub fn ensure_schema(&self, document: &str) -> Result<()> {
        self.conn.ensure_schema(document)?;
        Ok(())
    }

    /// 当前持久化的 VOS schema 文档；空库返回 `None`。
    pub fn schema_document(&self) -> Result<Option<String>> {
        Ok(self.conn.schema()?.map(|schema| schema.document))
    }

    /// 以新的 VOS 文档推进 schema 版本，保留既有类型与字段身份。
    ///
    /// 文档必须包含全部既有表：`migrate_schema` 只演进，不删除未列出的定义。
    /// 新库请先 [`Self::ensure_schema`]。
    pub fn migrate_schema(&self, document: &str) -> Result<()> {
        self.conn.migrate_schema(document, &RenameMap::default())?;
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
/// 覆盖标量、字节（tagged `bytes`）、UUID/时标（tagged 形态）、列表与嵌套行；
/// 数组映射为 `[T]` 列表值，对象映射为嵌套行。对象与数组都不是标量，但都是
/// VOS schema 声明过的容器形态，因此这里保留结构而不是静默丢字段。
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
        JsonValue::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                list.push(json_to_value(item)?);
            }
            Ok(Value::List(list))
        }
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
        Value::List(items) => JsonValue::Array(items.iter().map(value_to_json).collect()),
        // 向量 / 对象 / 文件不参与应用数据面；保底给出 kind 标签，避免 panic。
        other => json!({ "$database": "unsupported", "kind": other.kind() }),
    }
}
