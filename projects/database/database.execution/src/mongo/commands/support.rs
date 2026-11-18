//! 命令通用原语：错误信封、hello/ping、字段与索引条目助手、命名空间解析、
//! 游标应答、页扫描与排序键。

use super::*;

/// 聚合表达式求值端口：`$expr` 谓词复用聚合表达式求值器。
pub(crate) struct AggregationExpressions;

impl ExpressionContext for AggregationExpressions {
    fn evaluate(&self, document: &BsonDocument, expression: &BsonValue, variables: &ExpressionVariables) -> Result<BsonValue, CommandError> {
        evaluate_expression_vars(document, expression, variables)
    }
}

/// `hint` 解析需要的最小目录视图，读存储留在执行层。
pub(crate) struct StoreCatalog<'a>(&'a dyn MongoStorage);

impl IndexCatalog for StoreCatalog<'_> {
    fn namespace_exists(&self, namespace: &str) -> Result<bool, CommandError> {
        Ok(self.0.namespace_exists(namespace)?)
    }

    fn index_specs(&self, namespace: &str) -> Result<Vec<IndexSpec>, CommandError> {
        Ok(self.0.index_specs(namespace)?)
    }
}

pub(crate) fn resolve_hint(
    command: &BsonDocument,
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    sort: Option<&BsonValue>,
    projection: Option<&BsonValue>,
) -> Result<Hint, CommandError> {
    database_query::mongo::plan::resolve_hint(&StoreCatalog(store), command.get("hint"), namespace, filter, sort, projection)
}

pub(crate) fn document_matches(document: &BsonDocument, filter: &BsonDocument) -> Result<bool, CommandError> {
    matches(document, filter, &AggregationExpressions)
}

pub(crate) fn document_matches_vars(
    document: &BsonDocument,
    filter: &BsonDocument,
    variables: &ExpressionVariables,
) -> Result<bool, CommandError> {
    matches_with_variables(document, filter, variables, &AggregationExpressions)
}

/// Update operators this gateway implements.
pub const UPDATE_OPERATORS: [&str; 12] =
    ["$set", "$unset", "$inc", "$mul", "$min", "$max", "$rename", "$push", "$pop", "$addToSet", "$pull", "$setOnInsert"];

/// Result document for a failed command: `{ ok: 0, errmsg, code, codeName, ... }`.
///
/// `codeName` strings below were read from `mongod 8.0.9`, covering every code
/// the index commands, the index-build duplicate key path and the aggregation
/// stages can emit. Codes outside that set keep the three-field shape rather
/// than guessing a name.
pub fn error_document(error: &CommandError) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("ok".to_string(), BsonValue::Double(0.0));
    document.push("errmsg".to_string(), BsonValue::String(error.message.clone()));
    document.push("code".to_string(), BsonValue::Int32(error.code));
    if let Some(name) = code_name(error.code) {
        document.push("codeName".to_string(), BsonValue::String(name.to_string()));
    }
    for (key, value) in &error.details {
        document.push(key.clone(), value.clone());
    }
    document
}

/// Symbolic name `mongod` reports next to a numeric error code.
pub fn code_name(code: i32) -> Option<&'static str> {
    Some(match code {
        2 => "BadValue",
        9 => "FailedToParse",
        14 => "TypeMismatch",
        16 => "InvalidLength",
        13 => "Unauthorized",
        26 => "NamespaceNotFound",
        27 => "IndexNotFound",
        48 => "NamespaceExists",
        43 => "CursorNotFound",
        67 => "CannotCreateIndex",
        72 => "InvalidOptions",
        73 => "InvalidNamespace",
        85 => "IndexOptionsConflict",
        86 => "IndexKeySpecsConflict",
        11000 => "DuplicateKey",
        112 => "WriteConflict",
        117 => "ConflictingOperationInProgress",
        20 => "IllegalOperation",
        121 => "DocumentValidationFailure",
        175 => "QueryPlanKilled",
        251 => "NoSuchTransaction",
        225 => "TransactionTooOld",
        256 => "TransactionCommitted",
        66 => "ImmutableField",
        28 => "PathNotViable",
        16990 => "Location16990",
        17124 => "Location17124",
        40101 => "Location40101",
        40102 => "Location40102",
        40105 => "Location40105",
        40228 => "Location40228",
        40414 => "IDLFailedToParse",
        40415 => "IDLUnknownField",
        51132 => "Location51132",
        51178 => "Location51178",
        51181 => "Location51181",
        51183 => "Location51183",
        51186 => "Location51186",
        51187 => "Location51187",
        40148 => "Location40148",
        40149 => "Location40149",
        5255100 => "Location5255100",
        5255000 => "Location5255000",
        5371602 => "Location5371602",
        7158303 => "Location7158303",
        13113 => "MergeStageNoMatchingDocument",
        16764 => "Location16764",
        31303 => "Location31303",
        28745 => "Location28745",
        28746 => "Location28746",
        28747 => "Location28747",
        28748 => "Location28748",
        28749 => "Location28749",
        40600 => "Location40600",
        40601 => "Location40601",
        51047 => "Location51047",
        16410 => "Location16410",
        17053 => "Location17053",
        17072 => "Location17072",
        17276 => "Location17276",
        5733201 => "Location5733201",
        5733401 => "Location5733401",
        5733402 => "Location5733402",
        5733408 => "Location5733408",
        5733409 => "Location5733409",
        5946802 => "Location5946802",
        605001 => "Location605001",
        6050200 => "Location6050200",
        6050201 => "Location6050201",
        6050202 => "Location6050202",
        6050203 => "Location6050203",
        6050204 => "Location6050204",
        6053600 => "Location6053600",
        6586400 => "Location6586400",
        _ => return None,
    })
}

/// `hello` / `isMaster` handshake answer.
///
/// The static limits are the ones `mongod 8.0.9` reports for wire version 25.
/// Topology fields (`setName`, `hosts`, `topologyVersion`, `connectionId`) belong
/// to the listener, which owns the replica set state and connection counter.
pub fn hello() -> BsonDocument {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0);
    let mut document = BsonDocument::new();
    document.push("isWritablePrimary".to_string(), BsonValue::Bool(true));
    document.push("maxBsonObjectSize".to_string(), BsonValue::Int32(16_777_216));
    document.push("maxMessageSizeBytes".to_string(), BsonValue::Int32(48_000_000));
    document.push("maxWriteBatchSize".to_string(), BsonValue::Int32(100_000));
    document.push("localTime".to_string(), BsonValue::DateTime(now));
    document.push("logicalSessionTimeoutMinutes".to_string(), BsonValue::Int32(30));
    document.push("minWireVersion".to_string(), BsonValue::Int32(0));
    document.push("maxWireVersion".to_string(), BsonValue::Int32(25));
    document.push("readOnly".to_string(), BsonValue::Bool(false));
    document.push("ok".to_string(), BsonValue::Double(1.0));
    document
}

/// `ping` answer.
pub fn ping() -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("ok".to_string(), BsonValue::Double(1.0));
    document
}

/// Renders an `_id` the way MongoDB's duplicate-key message does.
pub fn describe_id(value: &BsonValue) -> String {
    match value {
        BsonValue::String(text) => format!("\"{text}\""),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => {
            if number.is_finite() && number.fract() == 0.0 {
                format!("{number:.1}")
            }
            else {
                number.to_string()
            }
        }
        BsonValue::ObjectId(bytes) => {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("ObjectId('{hex}')")
        }
        other => format!("{other:?}"),
    }
}

pub(crate) fn field_values(document: &BsonDocument, field: &str) -> Vec<BsonValue> {
    let candidates = collect_field(document, field);
    if candidates.is_empty() {
        // A missing field is indexed as `null`, which is what makes `{a: null}`
        // and an absent `a` collide in a unique index.
        return vec![BsonValue::Null];
    }
    let mut values = Vec::new();
    for candidate in candidates {
        match candidate {
            BsonValue::Array(items) => {
                if items.is_empty() {
                    // `mongod 8.0.9` indexes an empty array as BSON undefined:
                    // `{a: []}` twice collides as `dup key: { a: undefined }`.
                    values.push(BsonValue::Undefined);
                }
                else {
                    // Only one level is expanded, so a nested array stays one
                    // element: `{a: [[1, 2]]}` keys on the array `[1, 2]` itself.
                    values.extend(items.iter().cloned());
                }
            }
            other => values.push(other.clone()),
        }
    }
    values
}

/// Whether one key field is array-valued, which is what makes the index multikey.
pub(crate) fn field_array_valued(document: &BsonDocument, field: &str) -> bool {
    collect_field(document, field).iter().any(|candidate| matches!(candidate, BsonValue::Array(_)))
        || (field.contains('.') && path_traverses_array(document, field))
}

/// Whether a dotted path descends through an array before reaching its leaf.
pub(crate) fn path_traverses_array(document: &BsonDocument, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    match document.get(segments[0]) {
        Some(value) => traverses_array(value, &segments[1..]),
        None => false,
    }
}

pub(crate) fn traverses_array(value: &BsonValue, segments: &[&str]) -> bool {
    if segments.is_empty() {
        return false;
    }
    match value {
        BsonValue::Array(_) => true,
        BsonValue::Document(document) => document.get(segments[0]).map(|next| traverses_array(next, &segments[1..])).unwrap_or(false),
        _ => false,
    }
}

/// Every index key one document contributes, in key order and element order.
///
/// A compound key is the cartesian product of its fields. Duplicate elements
/// inside one document are collapsed, which is why `{a: [3, 3]}` can be stored
/// under a unique index while a second document with `a: 3` cannot.
pub(crate) fn index_entries(document: &BsonDocument, specification: &IndexSpec) -> Vec<Vec<BsonValue>> {
    if specification.is_sparse() && specification.key.fields.iter().all(|(field, _)| collect_field(document, field).is_empty()) {
        // A sparse index only holds documents that carry at least one of the
        // indexed fields. An explicit `null` counts as present.
        return Vec::new();
    }
    if let Some(filter) = &specification.partial_filter_expression
        && !document_matches(document, filter).unwrap_or(false)
    {
        return Vec::new();
    }
    let mut tuples: Vec<Vec<BsonValue>> = vec![Vec::new()];
    for (field, _) in &specification.key.fields {
        let values = field_values(document, field);
        let mut expanded = Vec::with_capacity(tuples.len() * values.len());
        for tuple in &tuples {
            for value in &values {
                let mut next = tuple.clone();
                next.push(value.clone());
                expanded.push(next);
            }
        }
        tuples = expanded;
    }
    if let Some(collation) = &specification.collation {
        let strength = collation.get("strength").and_then(integer_of).unwrap_or(3);
        if strength <= 2 {
            for tuple in &mut tuples {
                for value in tuple.iter_mut() {
                    if let BsonValue::String(text) = value {
                        *text = text.to_lowercase();
                    }
                }
            }
        }
    }
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    tuples.retain(|tuple| seen.insert(index_tuple(tuple)));
    tuples
}

/// Every encoded key one document contributes under one index.
pub(crate) fn tuples_of(document: &BsonDocument, specification: &IndexSpec) -> BTreeSet<Vec<u8>> {
    index_entries(document, specification).into_iter().map(|tuple| index_tuple(&tuple)).collect()
}

/// Code `171` for two array-valued fields under one compound index.
///
/// `mongod 8.0.9` names the two array fields in reverse key order: index
/// `{a: 1, b: 1}` over `{a: [1], b: [2]}` reports `[b] [a]` even when a third
/// field is also an array.
pub(crate) fn parallel_array_error(document: &BsonDocument, indexes: &[IndexSpec]) -> Option<CommandError> {
    for specification in indexes.iter().filter(|index| index.key.fields.len() >= 2) {
        let array_fields: Vec<&str> =
            specification.key.fields.iter().map(|(field, _)| field.as_str()).filter(|field| field_array_valued(document, field)).collect();
        if array_fields.len() >= 2 {
            return Some(CommandError::new(171, format!("cannot index parallel arrays [{}] [{}]", array_fields[1], array_fields[0])));
        }
    }
    None
}

pub(crate) fn duplicate_index_error(index: usize, namespace: &str, specification: &IndexSpec, values: &[BsonValue]) -> WriteError {
    let mut key_value = BsonDocument::new();
    for ((field, _), value) in specification.key.fields.iter().zip(values) {
        key_value.push(field.clone(), value.clone());
    }
    let rendered: Vec<String> =
        specification.key.fields.iter().zip(values).map(|((field, _), value)| format!("{field}: {}", render_shell_value(value))).collect();
    WriteError::duplicate(
        index,
        format!("E11000 duplicate key error collection: {namespace} index: {} dup key: {{ {} }}", specification.name, rendered.join(", ")),
        specification.key.clone(),
        key_value,
    )
}

/// 索引项是否已经真实落地。
///
/// 空索引可能来自旧目录未重建或中断的删除；此时无法区分「没有匹配」与
/// 「条目缺失」，查询层退回集合扫描，保证结果完整。
pub(crate) fn index_has_entries(store: &dyn MongoStorage, namespace: &str, specification: &IndexSpec) -> Result<bool, CommandError> {
    store
        .reserved_has_entries(&crate::mongo::storage::index_namespace(namespace, &specification.name))
        .map_err(|error| CommandError::new(1, format!("index lookup failed: {error}")))
}

/// 没有显式 hint 时按过滤条件挑一个可执行索引，并按目录顺序核对索引项已落地。
pub(crate) fn automatic_index(store: &dyn MongoStorage, namespace: &str, filter: &BsonDocument) -> Result<Option<IndexSpec>, CommandError> {
    let specs = store.index_specs(namespace)?;
    for specification in automatic_candidates(&specs, filter, &AggregationExpressions) {
        if index_has_entries(store, namespace, &specification)? {
            return Ok(Some(specification));
        }
    }
    Ok(None)
}

/// `<db>.$cmd` for a collection namespace, which is where `mongod` logs
/// collection and index commands.
pub(crate) fn command_namespace(namespace: &str) -> String {
    match namespace.split_once('.') {
        Some((database, _)) => format!("{database}.$cmd"),
        None => format!("{namespace}.$cmd"),
    }
}

pub(crate) fn namespace_from_command(document: &BsonDocument, command_key: &str) -> Option<String> {
    let database = document.database();
    let collection = match document.get(command_key) {
        Some(BsonValue::String(collection)) if valid_collection_name(collection) => collection.clone(),
        _ => return None,
    };
    Some(format!("{database}.{collection}"))
}

pub(crate) fn valid_collection_name(name: &str) -> bool {
    if name.is_empty() || name.contains('$') || name.contains('\0') {
        return false;
    }
    if name.starts_with('.') || name.ends_with('.') {
        return false;
    }
    !name.contains("..")
}

pub(crate) fn cursor_reply(namespace: &str, batch_key: &str, batch: Vec<BsonValue>, cursor_id: i64) -> BsonDocument {
    let mut cursor = BsonDocument::new();
    cursor.push(batch_key.to_string(), BsonValue::Array(batch));
    cursor.push("id".to_string(), BsonValue::Int64(cursor_id));
    cursor.push("ns".to_string(), BsonValue::String(namespace.to_string()));

    let mut response = BsonDocument::new();
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response.push("cursor".to_string(), BsonValue::Document(cursor));
    response
}

pub(crate) fn collect_matching(
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    session: Option<&str>,
) -> Result<Vec<BsonDocument>, CommandError> {
    if let Some(identifier) = single_id_lookup(filter) {
        let key = value_to_key(identifier);
        return match namespace_document(store, namespace, &key, session)? {
            Some(bytes) => match parse_bson(&bytes) {
                Ok(document) if document_matches(&document, filter)? => Ok(vec![document]),
                _ => Ok(Vec::new()),
            },
            None => Ok(Vec::new()),
        };
    }

    let mut documents = Vec::new();
    for (_key, value) in namespace_documents(store, namespace, session)? {
        if let Ok(candidate) = parse_bson(&value)
            && document_matches(&candidate, filter)?
        {
            documents.push(candidate);
        }
    }
    Ok(documents)
}

/// Page size of a stop-bounded collection scan: large enough that the page
/// boundary is amortised, small enough that an early stop does not clone the
/// whole collection.
pub(crate) const SCAN_PAGE: usize = 256;

/// 读取一页集合扫描，返回本页命中与下一页的排他游标。
///
/// `limit` 是这一页最多读取的文档数；读满一页才保留续扫位置，不足一页即到
/// 末尾。命中先按 `_id` 顺序收集，与整段扫描的交付顺序一致。
pub(crate) fn scan_collection_page(
    store: &dyn MongoStorage,
    namespace: &str,
    filter: &BsonDocument,
    limit: usize,
    after: Option<&[u8]>,
) -> Result<(Vec<BsonDocument>, Option<Vec<u8>>), CommandError> {
    let page = store.scan_page(namespace, after, limit).map_err(|error| CommandError::new(1, format!("scan failed: {error}")))?;
    let next = if page.len() < limit { None } else { page.last().map(|(key, _)| key.clone()) };
    let mut documents = Vec::new();
    for (_key, value) in page {
        if let Ok(document) = parse_bson(&value)
            && document_matches(&document, filter)?
        {
            documents.push(document);
        }
    }
    Ok((documents, next))
}

pub(crate) fn single_id_lookup(filter: &BsonDocument) -> Option<&BsonValue> {
    if filter.fields.len() != 1 {
        return None;
    }
    let (key, value) = filter.fields.first()?;
    if key != "_id" {
        return None;
    }
    match value {
        BsonValue::Document(operators) if operators.fields.iter().any(|(operator, _)| operator.starts_with('$')) => None,
        _ => Some(value),
    }
}

/// Sorts one stage input. Array fields use the lowest element ascending and the
/// highest element descending, as the `$sort` page specifies.
pub(crate) fn sort_key(document: &BsonDocument, path: &str, direction: i32) -> BsonValue {
    let value = lookup_field(document, path);
    let Some(value) = value
    else {
        // Missing fields compare like null; the manual states two documents that
        // both lack the field sort on "the same" value.
        return BsonValue::Null;
    };
    match value {
        BsonValue::Array(items) if !items.is_empty() => {
            let mut keys = items.iter();
            let mut chosen = keys.next().unwrap().clone();
            for item in keys {
                let ordering = compare_values(&chosen, item);
                if (direction >= 0 && ordering == Ordering::Greater) || (direction < 0 && ordering == Ordering::Less) {
                    chosen = item.clone();
                }
            }
            chosen
        }
        other => other.clone(),
    }
}

pub(crate) fn reply_count(reply: &BsonDocument, field: &str) -> i64 {
    match reply.get(field) {
        Some(BsonValue::Int32(value)) => i64::from(*value),
        Some(BsonValue::Int64(value)) => *value,
        Some(BsonValue::Double(value)) => *value as i64,
        _ => 0,
    }
}

pub(crate) fn parse_sort(sort: Option<&BsonValue>) -> Vec<(String, i32)> {
    match sort {
        Some(BsonValue::Document(document)) => document
            .fields
            .iter()
            .filter_map(|(field, direction)| int_value(direction).filter(|value| *value != 0).map(|value| (field.clone(), value as i32)))
            .collect(),
        _ => Vec::new(),
    }
}

/// One document's sort key: each field resolved once, with its direction.
///
/// Comparison walks the fields in `sort` order and reverses the per-field
/// ordering for a descending key, which is what [`sort_documents`] did inline.
pub(crate) struct DocumentSortKey(Vec<(BsonValue, i32)>);

impl Ord for DocumentSortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        for ((left, direction), (right, _)) in self.0.iter().zip(other.0.iter()) {
            let ordering = compare_values(left, right);
            if ordering != Ordering::Equal {
                return if *direction < 0 { ordering.reverse() } else { ordering };
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for DocumentSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for DocumentSortKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for DocumentSortKey {}

/// Stable sort by the requested keys.
///
/// Each document's key is resolved once (`sort_by_cached_key`) instead of on
/// both sides of every comparison, so an `n log n` sort does `n` field lookups
/// rather than `2 n log n`.
pub(crate) fn sort_documents(documents: &mut [BsonDocument], sort: &[(String, i32)]) {
    if sort.is_empty() {
        return;
    }
    documents.sort_by_cached_key(|document| {
        DocumentSortKey(sort.iter().map(|(field, direction)| (sort_key(document, field, *direction), *direction)).collect())
    });
}
