//! 集合选项与索引条目的 catalog 记录编码。
//!
//! 一份记录描述单个 `<database>.<collection>` 的选项与它自己的索引。`.yykv` 后端
//! 把它写进 `.yyds` catalog 的 schema 槽，KV 执行器后端把它写进预留的 `!catalog`
//! 命名空间，两边共用同一份字节格式。

use database_protocol_mongo::{
    bson::{BsonDocument, BsonValue},
    json,
};

/// Version of the product document stored in the `.yyds` schema slot.
pub const CATALOG_DOCUMENT_VERSION: u32 = 1;

/// One user-created index. `_id_` is implicit and is never stored here.
#[derive(Debug, Clone)]
pub struct IndexSpec {
    /// Namespace the index belongs to.
    pub namespace: String,
    /// Index name drivers and `listIndexes` report.
    pub name: String,
    /// Ordered key specification, exactly as it was sent.
    pub key: BsonDocument,
    /// Whether the index rejects duplicate keys.
    pub unique: bool,
    /// `sparse` exactly as requested. `mongod 8.0.9` reports an explicit
    /// `sparse: false` in `listIndexes`, so the requested value is kept.
    pub sparse: Option<bool>,
    /// `hidden: true` keeps the index out of the planner but in the catalog.
    pub hidden: bool,
    /// TTL expiry in seconds, applied to single-field date values.
    pub expire_after_seconds: Option<i32>,
    /// `collation` normalized the way `listIndexes` reports it.
    pub collation: Option<BsonDocument>,
    /// `partialFilterExpression`: only matching documents join the index.
    pub partial_filter_expression: Option<BsonDocument>,
    /// `wildcardProjection` of a wildcard index, as `listIndexes` reports it.
    pub wildcard_projection: Option<BsonDocument>,
    /// 索引字段出现过数组值。两侧范围谓词在 multikey 上不能按同一元素取区间。
    pub multikey: bool,
}

impl IndexSpec {
    /// Effective `sparse` flag.
    pub fn is_sparse(&self) -> bool {
        self.sparse.unwrap_or(false)
    }

    /// Whether the index is usable by the planner (`hidden` is not).
    pub fn is_visible(&self) -> bool {
        !self.hidden
    }

    /// One catalog entry. The key is stored as ordered `[field, value]` pairs so
    /// a compound index keeps its creation order across a reopen.
    pub fn to_json(&self) -> String {
        let field = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
        let mut text =
            format!("{{\"namespace\":{},\"name\":{},\"unique\":{},\"key\":[", field(&self.namespace), field(&self.name), self.unique);
        for (position, (name, value)) in self.key.fields.iter().enumerate() {
            if position > 0 {
                text.push(',');
            }
            text.push('[');
            text.push_str(&field(name));
            text.push(',');
            text.push_str(&serde_json::to_string(&json::to_json(value)).unwrap_or_else(|_| "null".to_string()));
            text.push(']');
        }
        text.push(']');
        if let Some(sparse) = self.sparse {
            text.push_str(&format!(",\"sparse\":{sparse}"));
        }
        if self.hidden {
            text.push_str(",\"hidden\":true");
        }
        if self.multikey {
            text.push_str(",\"multikey\":true");
        }
        else {
            // 显式写 false：旧目录没有这个字段，解析时按「可能 multikey」保守处理。
            text.push_str(",\"multikey\":false");
        }
        if let Some(seconds) = self.expire_after_seconds {
            text.push_str(&format!(",\"expireAfterSeconds\":{seconds}"));
        }
        if let Some(collation) = &self.collation {
            text.push_str(&format!(
                ",\"collation\":{}",
                serde_json::to_string(&json::to_json(&BsonValue::Document(collation.clone()))).unwrap_or_else(|_| "null".to_string())
            ));
        }
        if let Some(filter) = &self.partial_filter_expression {
            text.push_str(&format!(
                ",\"partialFilterExpression\":{}",
                serde_json::to_string(&json::to_json(&BsonValue::Document(filter.clone()))).unwrap_or_else(|_| "null".to_string())
            ));
        }
        if let Some(projection) = &self.wildcard_projection {
            text.push_str(&format!(
                ",\"wildcardProjection\":{}",
                serde_json::to_string(&json::to_json(&BsonValue::Document(projection.clone()))).unwrap_or_else(|_| "null".to_string())
            ));
        }
        text.push('}');
        text
    }
}

/// Collection-level options kept in the catalog. Only non-default values are
/// stored, so collections without validators keep the original catalog shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectionOptions {
    /// `$jsonSchema` (or other validator) exactly as it was sent.
    pub validator: Option<serde_json::Value>,
    /// `strict` (default), `moderate`, or `off`.
    pub validation_level: Option<String>,
    /// `error` (default) or `warn`.
    pub validation_action: Option<String>,
}

/// 索引项命名空间标记。
///
/// 用户可见的库名与集合名都不能包含 `$`，所以 `<collection>$idx$<index>`
/// 不会与用户集合冲突。索引项与文档共用同一个 shard，只是命名空间不进入目录。
pub const INDEX_NAMESPACE_MARKER: &str = "$idx$";

/// 该 namespace 是否属于索引项保留命名空间。
pub fn is_index_namespace(namespace: &str) -> bool {
    namespace.contains(INDEX_NAMESPACE_MARKER)
}

/// 由一个集合 namespace 与索引名拼出索引项 namespace。
pub fn index_namespace(namespace: &str, index: &str) -> String {
    format!("{namespace}{INDEX_NAMESPACE_MARKER}{index}")
}

/// Errors surfaced by the Mongo persistence contract.
#[derive(Debug)]
pub enum StoreError {
    /// The shard rejected the command.
    Shard(String),
    /// The catalog is missing pieces, malformed, or points at nothing.
    Catalog(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shard(message) => write!(formatter, "{message}"),
            Self::Catalog(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 把单个集合的选项与索引编码成后端无关的 catalog 记录。
///
/// `.yykv` 后端把它写进 `.yyds` catalog，执行器后端把它写进预留的
/// `!catalog` namespace，两者共用同一份字节格式，因此切换后端不需要重写元数据。
pub fn collection_record(namespace: &str, options: &CollectionOptions, indexes: &[IndexSpec]) -> String {
    let mut text = format!("{{\"version\":{CATALOG_DOCUMENT_VERSION},\"collections\":[");
    text.push_str(&collection_options_json(namespace, options));
    text.push_str("],\"indexes\":[");
    for (position, index) in indexes.iter().filter(|index| index.namespace == namespace).enumerate() {
        if position > 0 {
            text.push(',');
        }
        text.push_str(&index.to_json());
    }
    text.push_str("]}");
    text
}

/// 解析 [`collection_record`] 生成的记录，只返回该 namespace 自己的部分。
pub fn parse_collection_record(namespace: &str, text: &str) -> Result<(CollectionOptions, Vec<IndexSpec>), StoreError> {
    let options = parse_collections(text)?.remove(namespace).unwrap_or_default();
    let indexes = parse_indexes(text)?.into_iter().filter(|index| index.namespace == namespace).collect();
    Ok((options, indexes))
}

/// 把一个集合的选项编码成 catalog 的 `collections` 条目。
///
/// 顶层 `.yyds` catalog 文档与单集合 [`collection_record`] 共用这段编码。
pub fn collection_options_json(namespace: &str, options: &CollectionOptions) -> String {
    let field = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    let mut text = format!("{{\"namespace\":{}", field(namespace));
    if let Some(validator) = &options.validator {
        text.push_str(&format!(",\"validator\":{}", serde_json::to_string(validator).unwrap_or_else(|_| "null".to_string())));
    }
    if let Some(level) = &options.validation_level {
        text.push_str(&format!(",\"validationLevel\":{}", field(level)));
    }
    if let Some(action) = &options.validation_action {
        text.push_str(&format!(",\"validationAction\":{}", field(action)));
    }
    text.push('}');
    text
}

/// 解析 catalog 文档里的 `collections` 条目，缺段时返回空表。
pub fn parse_collections(document: &str) -> Result<std::collections::BTreeMap<String, CollectionOptions>, StoreError> {
    let value: serde_json::Value =
        serde_json::from_str(document).map_err(|error| StoreError::Catalog(format!("catalog schema document is not JSON: {error}")))?;
    let Some(entries) = value.get("collections")
    else {
        return Ok(std::collections::BTreeMap::new());
    };
    let Some(entries) = entries.as_array()
    else {
        return Err(StoreError::Catalog("catalog schema document has a non-array `collections` field".to_string()));
    };
    let mut collections = std::collections::BTreeMap::new();
    for entry in entries {
        let namespace = entry
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Catalog("catalog collection entry has no namespace".to_string()))?;
        let options = CollectionOptions {
            validator: entry.get("validator").cloned(),
            validation_level: entry.get("validationLevel").and_then(serde_json::Value::as_str).map(str::to_string),
            validation_action: entry.get("validationAction").and_then(serde_json::Value::as_str).map(str::to_string),
        };
        collections.insert(namespace.to_string(), options);
    }
    Ok(collections)
}

/// 解析 catalog 文档里的 `indexes` 条目，缺段时返回空表。
pub fn parse_indexes(document: &str) -> Result<Vec<IndexSpec>, StoreError> {
    let value: serde_json::Value =
        serde_json::from_str(document).map_err(|error| StoreError::Catalog(format!("catalog schema document is not JSON: {error}")))?;
    let Some(entries) = value.get("indexes")
    else {
        return Ok(Vec::new());
    };
    let Some(entries) = entries.as_array()
    else {
        return Err(StoreError::Catalog("catalog schema document has a non-array `indexes` field".to_string()));
    };
    let mut indexes = Vec::with_capacity(entries.len());
    for entry in entries {
        let namespace = entry
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Catalog("catalog index entry has no namespace".to_string()))?;
        let name =
            entry.get("name").and_then(serde_json::Value::as_str).ok_or_else(|| StoreError::Catalog("catalog index entry has no name".to_string()))?;
        let sparse = entry.get("sparse").and_then(serde_json::Value::as_bool);
        let hidden = entry.get("hidden").and_then(serde_json::Value::as_bool).unwrap_or(false);
        // 旧目录没有该字段时按 multikey 处理：宁可退回集合扫描，也不漏数组文档。
        let multikey = entry.get("multikey").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let expire_after_seconds = entry.get("expireAfterSeconds").and_then(serde_json::Value::as_i64).map(|seconds| seconds as i32);
        let collation = match entry.get("collation") {
            Some(value) => Some(
                document_option(value).map_err(|error| StoreError::Catalog(format!("catalog index `{name}` collation is invalid: {error}")))?,
            ),
            None => None,
        };
        let partial_filter_expression = match entry.get("partialFilterExpression") {
            Some(value) => Some(
                document_option(value)
                    .map_err(|error| StoreError::Catalog(format!("catalog index `{name}` partial filter is invalid: {error}")))?,
            ),
            None => None,
        };
        let wildcard_projection = match entry.get("wildcardProjection") {
            Some(value) => Some(
                document_option(value)
                    .map_err(|error| StoreError::Catalog(format!("catalog index `{name}` wildcard projection is invalid: {error}")))?,
            ),
            None => None,
        };
        let Some(pairs) = entry.get("key").and_then(serde_json::Value::as_array)
        else {
            return Err(StoreError::Catalog(format!("catalog index `{name}` has no key array")));
        };
        let mut key = BsonDocument::new();
        for pair in pairs {
            let Some(fields) = pair.as_array()
            else {
                return Err(StoreError::Catalog(format!("catalog index `{name}` has a malformed key entry")));
            };
            if fields.len() != 2 {
                return Err(StoreError::Catalog(format!("catalog index `{name}` has a malformed key entry")));
            }
            let Some(field) = fields[0].as_str()
            else {
                return Err(StoreError::Catalog(format!("catalog index `{name}` has a non-string key field")));
            };
            let value = json::from_json(&fields[1])
                .map_err(|error| StoreError::Catalog(format!("catalog index `{name}` key value is invalid: {error}")))?;
            key.push(field.to_string(), value);
        }
        let unique = entry.get("unique").and_then(serde_json::Value::as_bool).unwrap_or(false);
        indexes.push(IndexSpec {
            namespace: namespace.to_string(),
            name: name.to_string(),
            key,
            unique,
            sparse,
            hidden,
            expire_after_seconds,
            collation,
            partial_filter_expression,
            wildcard_projection,
            multikey,
        });
    }
    Ok(indexes)
}

fn document_option(value: &serde_json::Value) -> Result<BsonDocument, String> {
    match json::from_json(value) {
        Ok(BsonValue::Document(document)) => Ok(document),
        Ok(_) => Err("not a document".to_string()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database_protocol_mongo::bson::BsonValue;

    fn index(namespace: &str, name: &str) -> IndexSpec {
        let mut key = BsonDocument::new();
        key.push("a".to_string(), BsonValue::Int32(1));
        IndexSpec {
            namespace: namespace.to_string(),
            name: name.to_string(),
            key,
            unique: true,
            sparse: Some(false),
            hidden: true,
            expire_after_seconds: Some(5),
            collation: None,
            partial_filter_expression: None,
            wildcard_projection: None,
            multikey: false,
        }
    }

    #[test]
    fn record_round_trips_options_and_indexes() {
        let options = CollectionOptions {
            validator: Some(serde_json::json!({"b": {"$exists": true}})),
            validation_level: Some("moderate".to_string()),
            validation_action: Some("warn".to_string()),
        };
        let record = collection_record("app.items", &options, &[index("app.items", "a_1"), index("other.items", "b_1")]);
        let (parsed_options, parsed_indexes) = parse_collection_record("app.items", &record).unwrap();
        assert_eq!(parsed_options, options);
        assert_eq!(parsed_indexes.len(), 1);
        assert_eq!(parsed_indexes[0].name, "a_1");
        assert!(parsed_indexes[0].unique);
        assert!(parsed_indexes[0].is_visible() == false);
        assert_eq!(parsed_indexes[0].expire_after_seconds, Some(5));
    }

    #[test]
    fn default_options_keep_the_original_record_shape() {
        let record = collection_record("app.items", &CollectionOptions::default(), &[]);
        assert_eq!(record, "{\"version\":1,\"collections\":[{\"namespace\":\"app.items\"}],\"indexes\":[]}");
    }

    #[test]
    fn index_namespaces_never_collide_with_user_collections() {
        assert_eq!(index_namespace("app.items", "a_1"), "app.items$idx$a_1");
        assert!(is_index_namespace("app.items$idx$a_1"));
        assert!(!is_index_namespace("app.items"));
    }
}
