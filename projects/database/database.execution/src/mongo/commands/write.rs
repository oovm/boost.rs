//! 写命令：`insert` / `update` / `delete` / `findAndModify` / `bulkWrite`、写索引维护与事务暂存写入。

use super::*;

/// One `writeErrors` entry. Field order follows `mongod 8.0.9`: `index`, `code`,
/// `errmsg`, then `keyPattern` / `keyValue` for duplicate-key failures.
pub(crate) struct WriteError {
    pub(crate) index: usize,
    pub(crate) code: i32,
    pub(crate) message: String,
    pub(crate) key_pattern: Option<BsonDocument>,
    pub(crate) key_value: Option<BsonDocument>,
    pub(crate) err_info: Option<BsonDocument>,
}

impl WriteError {
    fn plain(index: usize, code: i32, message: impl Into<String>) -> Self {
        Self { index, code, message: message.into(), key_pattern: None, key_value: None, err_info: None }
    }

    fn validation(index: usize, err_info: BsonDocument) -> Self {
        Self {
            index,
            code: 121,
            message: "Document failed validation".to_string(),
            key_pattern: None,
            key_value: None,
            err_info: Some(err_info),
        }
    }

    pub(crate) fn duplicate(index: usize, message: impl Into<String>, key_pattern: BsonDocument, key_value: BsonDocument) -> Self {
        Self { index, code: 11000, message: message.into(), key_pattern: Some(key_pattern), key_value: Some(key_value), err_info: None }
    }
}

/// Largest BSON object `mongod 8.0.9` accepts, as its own error message states
/// (`Size must be between 0 and 16793600(16MB)`).
pub(crate) const MAX_BSON_OBJECT_SIZE: usize = 16_793_600;

/// Rejects one stored document above the BSON ceiling before it reaches the
/// store, with the code and wording `mongod 8.0.9` uses.
pub(crate) fn ensure_document_size(document: &BsonDocument, collection: &str) -> Result<(), CommandError> {
    let size = document.encoded_len();
    if size > MAX_BSON_OBJECT_SIZE {
        return Err(CommandError::new(
            10334,
            format!(
                "BSONObj size: {size} (0x{size:X}) is invalid. Size must be between 0 and 16793600(16MB) First element: insert: \"{collection}\""
            ),
        ));
    }
    Ok(())
}

/// `insert`, with `_id` generation, `11000` duplicate mapping, unique-index
/// enforcement, and ordered / unordered continuation.
pub fn insert(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "insert").ok_or_else(|| CommandError::new(73, "insert command missing collection name"))?;
    reject_transaction_write_concern(command)?;
    let session = SessionContext::new(command, store)?;

    let Some(BsonValue::Array(documents)) = command.get("documents")
    else {
        return Err(CommandError::new(2, "insert requires a 'documents' array"));
    };
    if documents.is_empty() || documents.len() > 100_000 {
        return Err(CommandError::new(16, format!("Write batch sizes must be between 1 and 100000. Got {} operations.", documents.len())));
    }
    let collection = namespace.rsplit_once('.').map(|(_, name)| name).unwrap_or(&namespace);
    for item in documents {
        if let BsonValue::Document(document) = item {
            ensure_document_size(document, collection)?;
        }
    }

    let ordered = match command.get("ordered") {
        Some(BsonValue::Bool(ordered)) => *ordered,
        _ => true,
    };

    let index_specs: Vec<IndexSpec> = store.index_specs(&namespace)?;
    let unique_indexes: Vec<IndexSpec> = index_specs.iter().filter(|index| index.unique).cloned().collect();
    let mut candidates: Vec<(usize, BsonDocument, BsonValue)> = Vec::with_capacity(documents.len());
    let mut write_errors: Vec<WriteError> = Vec::new();

    for (index, item) in documents.iter().enumerate() {
        let BsonValue::Document(sub_document) = item
        else {
            write_errors.push(WriteError::plain(index, 2, "documents must be objects"));
            if ordered {
                break;
            }
            continue;
        };
        let mut stored = sub_document.clone();
        if !stored.fields.iter().any(|(field, _)| field == "_id") {
            stored.push("_id".to_string(), BsonValue::ObjectId(generate_object_id()));
        }
        if let Some(error) = parallel_array_error(&stored, &index_specs) {
            write_errors.push(WriteError::plain(index, error.code, error.message));
            if ordered {
                break;
            }
            continue;
        }
        if let Some(info) = insert_validation_failure(store, &namespace, &stored)? {
            write_errors.push(WriteError::validation(index, info));
            if ordered {
                break;
            }
            continue;
        }
        let Some(identifier) = stored.fields.iter().find(|(field, _)| field == "_id").map(|(_, value)| value.clone())
        else {
            continue;
        };
        candidates.push((index, stored, identifier));
    }

    let mut accepted_documents: Vec<BsonDocument> = Vec::new();
    let inserted = if unique_indexes.is_empty() && session.key().is_none() {
        let entries: Vec<(Vec<u8>, Vec<u8>)> =
            candidates.iter().map(|(_, stored, identifier)| (value_to_key(identifier), encode_bson(stored))).collect();
        let positions: Vec<usize> = candidates.iter().map(|(index, _, _)| *index).collect();
        let identifiers: Vec<BsonValue> = candidates.iter().map(|(_, _, identifier)| identifier.clone()).collect();
        let report = store.insert(&namespace, &entries, ordered).map_err(|error| CommandError::new(2, format!("insert failed: {error}")))?;
        for duplicate in &report.duplicates {
            write_errors.push(id_duplicate_error(&namespace, positions[*duplicate], &identifiers[*duplicate]));
        }
        for (position, (_, stored, _)) in candidates.iter().enumerate() {
            if report.duplicates.contains(&position) {
                if ordered {
                    break;
                }
                continue;
            }
            accepted_documents.push(stored.clone());
        }
        if !index_specs.is_empty() {
            let pairs: Vec<(BsonDocument, Vec<u8>)> = accepted_documents
                .iter()
                .filter_map(|document| document.get("_id").map(|identifier| (document.clone(), value_to_key(identifier))))
                .collect();
            write_index_entries(store, session.key(), &pairs, &index_specs)?;
        }
        report.inserted as usize
    }
    else {
        let (count, accepted) = commit_insert(store, session.key(), &namespace, &candidates, &unique_indexes, ordered, &mut write_errors)?;
        accepted_documents.extend(accepted);
        count
    };
    write_errors.sort_by_key(|error| error.index);
    if session.key().is_none() {
        stage_insert_oplog(store, &namespace, &accepted_documents);
    }

    Ok(write_reply(inserted as i64, None, &write_errors))
}

/// Inserts the candidate documents that satisfy every unique index.
///
/// Documents are evaluated in request order. A rejected document never
/// contributes keys, so an unordered batch may still accept a later document
/// that reuses a rejected one's key, exactly like `mongod`.
///
/// 唯一性判定走已经落盘的索引项：`_id` 用点读，唯一索引按元组前缀探针，不再
/// 为了查重扫全表。只有索引项为空（旧目录还没回填）时才退回整表扫描，宁可慢
/// 也不能因为条目缺失放行重复键。
pub(crate) fn commit_insert(
    store: &dyn MongoStorage,
    session: Option<&str>,
    namespace: &str,
    candidates: &[(usize, BsonDocument, BsonValue)],
    unique_indexes: &[IndexSpec],
    ordered: bool,
    write_errors: &mut Vec<WriteError>,
) -> Result<(usize, Vec<BsonDocument>), CommandError> {
    let mut indexed: Vec<BTreeSet<Vec<u8>>> = vec![BTreeSet::new(); unique_indexes.len()];
    let mut persistent: Vec<bool> = Vec::with_capacity(unique_indexes.len());
    for specification in unique_indexes {
        let entries = store
            .reserved_has_entries(&crate::mongo::storage::index_namespace(namespace, &specification.name))
            .map_err(|error| CommandError::new(1, format!("index probe failed: {error}")))?;
        persistent.push(entries);
    }
    let fallback: Vec<usize> = persistent.iter().enumerate().filter(|(_, entries)| !**entries).map(|(position, _)| position).collect();
    if !fallback.is_empty() {
        for (_, payload) in namespace_documents(store, namespace, session)? {
            let Ok(document) = parse_bson(&payload)
            else {
                continue;
            };
            for position in &fallback {
                for tuple in index_entries(&document, &unique_indexes[*position]) {
                    indexed[*position].insert(index_tuple(&tuple));
                }
            }
        }
    }

    let mut staged: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut accepted: Vec<(BsonDocument, Vec<u8>)> = Vec::new();
    for (index, document, identifier) in candidates {
        let id_key = value_to_key(identifier);
        let taken = staged.contains(&id_key) || namespace_document(store, namespace, &id_key, session)?.is_some();
        if taken {
            write_errors.push(id_duplicate_error(namespace, *index, identifier));
            if ordered {
                break;
            }
            continue;
        }
        let mut conflict: Option<(&IndexSpec, Vec<BsonValue>)> = None;
        'specification: for (position, specification) in unique_indexes.iter().enumerate() {
            for values in index_entries(document, specification) {
                let tuple = index_tuple(&values);
                if indexed[position].contains(&tuple) {
                    conflict = Some((specification, values));
                    break 'specification;
                }
                if persistent[position] {
                    let probe = store
                        .scan_reserved(&crate::mongo::storage::index_namespace(namespace, &specification.name), &tuple)
                        .map_err(|error| CommandError::new(1, format!("index probe failed: {error}")))?;
                    if !probe.is_empty() {
                        conflict = Some((specification, values));
                        break 'specification;
                    }
                }
            }
        }
        if let Some((specification, values)) = conflict {
            write_errors.push(duplicate_index_error(*index, namespace, specification, &values));
            if ordered {
                break;
            }
            continue;
        }
        staged.insert(id_key.clone());
        for (position, specification) in unique_indexes.iter().enumerate() {
            for tuple in index_entries(document, specification) {
                indexed[position].insert(index_tuple(&tuple));
            }
        }
        accepted.push((document.clone(), id_key));
    }

    if !accepted.is_empty() {
        stage_insert(store, session, namespace, &accepted)?;
    }
    Ok((accepted.len(), accepted.into_iter().map(|(document, _)| document).collect()))
}

/// Stages the oplog entries for documents an insert actually wrote.
///
/// One document is a plain `i` entry; a batch is a single `applyOps` command
/// entry, which is what `mongod 8.0.9` writes for a multi-document insert.
pub(crate) fn stage_insert_oplog(store: &dyn MongoStorage, namespace: &str, documents: &[BsonDocument]) {
    if documents.is_empty() || !store.oplog_enabled() {
        return;
    }
    let uuid = collection_uuid(namespace);
    let entries: Vec<OplogEntry> = documents.iter().map(|document| OplogEntry::insert(namespace, uuid.clone(), document)).collect();
    if entries.len() == 1 {
        if let Some(entry) = entries.into_iter().next() {
            store.stage_oplog(entry);
        }
    }
    else {
        store.stage_oplog(OplogEntry::apply_ops(entries));
    }
}

pub(crate) fn id_duplicate_error(namespace: &str, index: usize, identifier: &BsonValue) -> WriteError {
    let mut key_pattern = BsonDocument::new();
    key_pattern.push("_id".to_string(), BsonValue::Int32(1));
    let mut key_value = BsonDocument::new();
    key_value.push("_id".to_string(), identifier.clone());
    WriteError::duplicate(
        index,
        format!("E11000 duplicate key error collection: {namespace} index: _id_ dup key: {{ _id: {} }}", describe_id(identifier)),
        key_pattern,
        key_value,
    )
}

/// The index values one field contributes, expanding an array one level.

/// `delete` with `limit` (`0` deletes every match).
pub fn delete(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "delete").ok_or_else(|| CommandError::new(73, "delete command missing collection name"))?;
    reject_transaction_write_concern(command)?;
    let session = SessionContext::new(command, store)?;

    let specifications = match command.get("deletes") {
        Some(BsonValue::Array(deletes)) => deletes.clone(),
        _ => {
            let mut single = BsonDocument::new();
            single.push("q".to_string(), BsonValue::Document(BsonDocument::new()));
            single.push("limit".to_string(), BsonValue::Int32(0));
            vec![BsonValue::Document(single)]
        }
    };

    let mut deleted: i64 = 0;
    let mut deleted_identifiers: Vec<BsonValue> = Vec::new();
    for specification in &specifications {
        let BsonValue::Document(specification) = specification
        else {
            continue;
        };
        let empty = BsonDocument::new();
        let filter = match specification.get("q") {
            Some(BsonValue::Document(filter)) => filter,
            _ => &empty,
        };
        let limit = match specification.get("limit") {
            Some(BsonValue::Int32(limit)) if *limit >= 0 => *limit as usize,
            _ => 0,
        };
        validate_filter(filter, "delete")?;

        if let Some(identifier) = single_id_lookup(filter) {
            let key = value_to_key(identifier);
            let removed = stage_delete(store, session.key(), &namespace, std::slice::from_ref(&key))?;
            deleted += removed as i64;
            if removed > 0 {
                deleted_identifiers.push(identifier.clone());
            }
        }
        else {
            let mut keys = Vec::new();
            let mut identifiers = Vec::new();
            for (key, value) in namespace_documents(store, &namespace, session.key())? {
                if limit > 0 && keys.len() >= limit {
                    break;
                }
                let Ok(candidate) = parse_bson(&value)
                else {
                    continue;
                };
                if document_matches(&candidate, filter)? {
                    if let Some(identifier) = candidate.get("_id") {
                        identifiers.push(identifier.clone());
                    }
                    keys.push(key);
                }
            }
            if !keys.is_empty() {
                deleted += stage_delete(store, session.key(), &namespace, &keys)? as i64;
                deleted_identifiers.extend(identifiers);
            }
        }
    }
    if session.key().is_none() {
        stage_delete_oplog(store, &namespace, &deleted_identifiers);
    }

    Ok(write_reply(deleted, None, &[]))
}

/// Stages one `d` entry per deleted document.
pub(crate) fn stage_delete_oplog(store: &dyn MongoStorage, namespace: &str, identifiers: &[BsonValue]) {
    if identifiers.is_empty() || !store.oplog_enabled() {
        return;
    }
    let uuid = collection_uuid(namespace);
    for identifier in identifiers {
        store.stage_oplog(OplogEntry::delete(namespace, uuid.clone(), identifier));
    }
}

/// Stages one `u` entry per document an update actually changed.
pub(crate) fn stage_update_oplog(store: &dyn MongoStorage, session: Option<&str>, namespace: &str, pairs: &[(BsonDocument, BsonDocument)]) {
    if pairs.is_empty() || session.is_some() || !store.oplog_enabled() {
        return;
    }
    let uuid = collection_uuid(namespace);
    for (old, new) in pairs {
        store.stage_oplog(OplogEntry::update(namespace, uuid.clone(), old, new));
    }
}

/// `update` with the documented update operators, replacement documents and
/// `upsert`; `multi` toggles updateMany.
pub fn update(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "update").ok_or_else(|| CommandError::new(73, "update command missing collection name"))?;
    reject_transaction_write_concern(command)?;
    let session = SessionContext::new(command, store)?;
    let Some(BsonValue::Array(specifications)) = command.get("updates")
    else {
        return Err(CommandError::new(2, "update requires an 'updates' array"));
    };

    let mut matched: i64 = 0;
    let mut modified: i64 = 0;
    let index_specs: Vec<IndexSpec> = store.index_specs(&namespace)?;
    let unique_indexes: Vec<IndexSpec> = index_specs.iter().filter(|index| index.unique).cloned().collect();
    // 唯一索引项全部已落盘且不在事务里时，冲突检查走前缀探针，不再为每条更新命令整表建 map。
    // 条目缺失（旧目录还没回填）或事务内暂存对探针不可见时，退回整表 map，宁可慢也不放行重复键。
    let probe_namespaces: Option<Vec<String>> = if session.key().is_some() {
        None
    }
    else {
        let mut namespaces = Vec::with_capacity(unique_indexes.len());
        let mut ready = true;
        for specification in &unique_indexes {
            let entries_namespace = crate::mongo::storage::index_namespace(&namespace, &specification.name);
            let entries =
                store.reserved_has_entries(&entries_namespace).map_err(|error| CommandError::new(1, format!("index probe failed: {error}")))?;
            if !entries {
                ready = false;
                break;
            }
            namespaces.push(entries_namespace);
        }
        ready.then_some(namespaces)
    };
    let mut indexed: Option<BTreeMap<Vec<u8>, Vec<BTreeSet<Vec<u8>>>>> = None;
    for (index, specification) in specifications.iter().enumerate() {
        let BsonValue::Document(specification) = specification
        else {
            return Err(CommandError::new(2, "update specification must be a document"));
        };
        let empty = BsonDocument::new();
        let filter = match specification.get("q") {
            Some(BsonValue::Document(filter)) => filter,
            _ => &empty,
        };
        validate_filter(filter, "update")?;

        let Some(BsonValue::Document(update_document)) = specification.get("u")
        else {
            return Err(CommandError::new(2, "update specification requires a 'u' document"));
        };
        let operator_update = update_document.fields.iter().any(|(field, _)| field.starts_with('$'));
        let replacement = !operator_update;
        if operator_update {
            if let Some((field, _)) = update_document.fields.iter().find(|(field, _)| !field.starts_with('$')) {
                return Err(CommandError::new(
                    9,
                    format!("Unknown modifier: {field}. Expected a valid update modifier or pipeline-style update specified as an array"),
                ));
            }
            validate_update_document(update_document)?;
        }
        let upsert = matches!(specification.get("upsert"), Some(BsonValue::Bool(true)));
        let multi = matches!(specification.get("multi"), Some(BsonValue::Bool(true)));

        let mut changes = Vec::new();
        let mut oplog_pairs: Vec<(BsonDocument, BsonDocument)> = Vec::new();
        // `_id` 等值过滤最多命中一篇，点读定位，不再整命名空间扫描匹配；探针不可用时退回扫描，
        // 扫描路径需要唯一 map 时惰性建一次，后续规格复用并就地刷新。
        let point_read = probe_namespaces.is_some() && single_id_lookup(filter).is_some();
        let candidates: Vec<(Vec<u8>, Vec<u8>)> = if let Some(identifier) = single_id_lookup(filter).filter(|_| point_read) {
            let key = value_to_key(identifier);
            match namespace_document(store, &namespace, &key, session.key())? {
                Some(payload) => vec![(key, payload)],
                None => Vec::new(),
            }
        }
        else {
            if !unique_indexes.is_empty() && indexed.is_none() {
                let mut map = BTreeMap::new();
                for (key, value) in namespace_documents(store, &namespace, session.key())? {
                    if let Ok(document) = parse_bson(&value) {
                        map.insert(key, unique_indexes.iter().map(|spec| tuples_of(&document, spec)).collect());
                    }
                }
                indexed = Some(map);
            }
            namespace_documents(store, &namespace, session.key())?
        };
        for (key, value) in candidates {
            let Ok(document) = parse_bson(&value)
            else {
                continue;
            };
            if !document_matches(&document, filter)? {
                continue;
            }
            matched += 1;
            let identifier_value = document.get("_id").cloned();
            let identifier = identifier_value.as_ref().map(describe_id).unwrap_or_else(|| "<missing>".to_string());
            let applied = if replacement {
                build_replacement(update_document, identifier_value.as_ref())
            }
            else {
                apply_update(&document, update_document, &identifier, filter, false)
            };
            let updated = match applied {
                Ok(updated) => updated,
                Err(error) => {
                    // The real server keeps the writes already applied, reports a
                    // zeroed counter pair plus `writeErrors`, and aborts the rest
                    // of the command.
                    if !changes.is_empty() {
                        stage_replace(store, session.key(), &namespace, &changes)?;
                    }
                    stage_update_oplog(store, session.key(), &namespace, &oplog_pairs);
                    return Ok(write_reply(0, Some(0), &[WriteError::plain(index, error.code, error.message.clone())]));
                }
            };
            if let Some(info) = update_validation_failure(store, &namespace, &document, &updated)? {
                if !changes.is_empty() {
                    stage_replace(store, session.key(), &namespace, &changes)?;
                }
                stage_update_oplog(store, session.key(), &namespace, &oplog_pairs);
                return Ok(write_reply(0, Some(0), &[WriteError::validation(index, info)]));
            }
            if let Some(error) = parallel_array_error(&updated, &index_specs) {
                // A parallel-array key cannot be indexed at all, so the real
                // server reports a zeroed counter pair plus `writeErrors`.
                if !changes.is_empty() {
                    stage_replace(store, session.key(), &namespace, &changes)?;
                }
                stage_update_oplog(store, session.key(), &namespace, &oplog_pairs);
                return Ok(write_reply(0, Some(0), &[WriteError::plain(index, error.code, error.message)]));
            }
            if !unique_indexes.is_empty() {
                let conflict = match (point_read, &probe_namespaces, &indexed) {
                    (true, Some(probes), _) => unique_conflict_by_probe(store, probes, &unique_indexes, &updated, &key)?,
                    (false, _, Some(map)) => find_unique_conflict(&unique_indexes, &updated, &key, map),
                    _ => None,
                };
                if let Some((specification, values)) = conflict {
                    // Same shape as a failing multi-update: already applied writes
                    // stay, counters report zero, and the command stops here.
                    if !changes.is_empty() {
                        stage_replace(store, session.key(), &namespace, &changes)?;
                    }
                    stage_update_oplog(store, session.key(), &namespace, &oplog_pairs);
                    return Ok(write_reply(0, Some(0), &[duplicate_index_error(index, &namespace, specification, &values)]));
                }
                if let Some(entry) = indexed.as_mut().and_then(|map| map.get_mut(&key)) {
                    *entry = unique_indexes.iter().map(|spec| tuples_of(&updated, spec)).collect();
                }
            }
            let payload = encode_bson(&updated);
            if payload != value {
                modified += 1;
                changes.push((key, payload));
                oplog_pairs.push((document, updated));
            }
            if !multi {
                break;
            }
        }
        if !changes.is_empty() {
            stage_replace(store, session.key(), &namespace, &changes)?;
        }
        stage_update_oplog(store, session.key(), &namespace, &oplog_pairs);
        if matched == 0 && upsert {
            let applied = if replacement {
                let query_identifier = match filter.get("_id") {
                    Some(BsonValue::Document(operators)) if has_operator(operators) => None,
                    Some(value) => Some(value.clone()),
                    None => None,
                };
                build_replacement(update_document, query_identifier.as_ref())
            }
            else {
                let base = upsert_base(filter);
                let identifier = base.get("_id").map(describe_id).unwrap_or_else(|| "<missing>".to_string());
                apply_update(&base, update_document, &identifier, filter, true)
            };
            let mut inserted = match applied {
                Ok(document) => document,
                Err(error) => {
                    return Ok(write_reply(0, Some(0), &[WriteError::plain(index, error.code, error.message)]));
                }
            };
            if inserted.get("_id").is_none() {
                inserted.fields.insert(0, ("_id".to_string(), BsonValue::ObjectId(generate_object_id())));
            }
            if let Some(info) = insert_validation_failure(store, &namespace, &inserted)? {
                return Ok(write_reply(0, Some(0), &[WriteError::validation(index, info)]));
            }
            let Some(identifier) = inserted.get("_id").cloned()
            else {
                return Ok(write_reply(0, Some(0), &[]));
            };
            if let Some(error) = parallel_array_error(&inserted, &index_specs) {
                return Ok(write_reply(0, Some(0), &[WriteError::plain(index, error.code, error.message)]));
            }
            let candidates = vec![(index, inserted, identifier.clone())];
            let mut write_errors = Vec::new();
            let (inserted_count, accepted) =
                commit_insert(store, session.key(), &namespace, &candidates, &unique_indexes, true, &mut write_errors)?;
            if inserted_count == 1 {
                if session.key().is_none() {
                    stage_insert_oplog(store, &namespace, &accepted);
                }
                return Ok(upsert_reply(index, &identifier));
            }
            return Ok(write_reply(0, Some(0), &write_errors));
        }
    }

    Ok(write_reply(matched, Some(modified), &[]))
}

/// Base document for an operator upsert: the equality clauses of the filter,
/// in filter order. Operator clauses and logical clauses are skipped, matching
/// the server's upsert construction.
pub(crate) fn upsert_base(filter: &BsonDocument) -> BsonDocument {
    let mut base = BsonDocument::new();
    for (field, value) in &filter.fields {
        if field.starts_with('$') {
            continue;
        }
        if let BsonValue::Document(operators) = value
            && has_operator(operators)
        {
            continue;
        }
        let segments: Vec<&str> = field.split('.').collect();
        let _ = set_segments(&mut base, &segments, value.clone());
    }
    base
}

/// Replacement document rule: `_id` is kept from the matched or queried
/// document unless the replacement repeats it, and an attempt to change it is
/// rejected with code `66`. Other fields are replaced wholesale.
pub(crate) fn build_replacement(specification: &BsonDocument, identifier: Option<&BsonValue>) -> Result<BsonDocument, CommandError> {
    let replacement_identifier = specification.get("_id");
    if let (Some(current), Some(replacement)) = (identifier, replacement_identifier)
        && !values_equal(Some(current), replacement)
    {
        return Err(CommandError::new(
            66,
            format!(
                "After applying the update, the (immutable) field '_id' was found to have been altered to _id: {}",
                describe_id(replacement)
            ),
        ));
    }
    let chosen = identifier.cloned().or_else(|| replacement_identifier.cloned());
    let mut result = BsonDocument::new();
    if let Some(identifier) = chosen {
        result.push("_id".to_string(), identifier);
    }
    for (field, value) in &specification.fields {
        if field != "_id" {
            result.push(field.clone(), value.clone());
        }
    }
    Ok(result)
}

/// Reply for an upsert that inserted one document: `n`, `upserted` and a zero
/// `nModified`, in the order the server emits them.
pub(crate) fn upsert_reply(index: usize, identifier: &BsonValue) -> BsonDocument {
    let mut response = BsonDocument::new();
    response.push("n".to_string(), count_value(1));
    let mut entry = BsonDocument::new();
    entry.push("index".to_string(), BsonValue::Int32(index as i32));
    entry.push("_id".to_string(), identifier.clone());
    response.push("upserted".to_string(), BsonValue::Array(vec![BsonValue::Document(entry)]));
    response.push("nModified".to_string(), count_value(0));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response
}

/// Finds the first unique index key of `document` already held by a different
/// stored document.
pub(crate) fn find_unique_conflict<'a>(
    indexes: &'a [IndexSpec],
    document: &BsonDocument,
    key: &[u8],
    current: &BTreeMap<Vec<u8>, Vec<BTreeSet<Vec<u8>>>>,
) -> Option<(&'a IndexSpec, Vec<BsonValue>)> {
    for (position, specification) in indexes.iter().enumerate() {
        for values in index_entries(document, specification) {
            let tuple = index_tuple(&values);
            for (other, sets) in current {
                if other.as_slice() != key && sets.get(position).is_some_and(|set| set.contains(&tuple)) {
                    return Some((specification, values));
                }
            }
        }
    }
    None
}

/// 按已落盘索引项探针检查 `document` 更新后的唯一键是否与**其它**文档冲突。
///
/// 每个唯一索引项的键是 `index_tuple(values) + 文档键`，条目按索引命名空间隔离，
/// 定长元组自定界，所以前缀 `tuple` 命中的条目尾部就是持有该元组的文档键；只要存在
/// 一条不等于本篇自身条目，即为冲突。探针只读，不整表扫描。
pub(crate) fn unique_conflict_by_probe<'a>(
    store: &dyn MongoStorage,
    namespaces: &[String],
    indexes: &'a [IndexSpec],
    document: &BsonDocument,
    key: &[u8],
) -> Result<Option<(&'a IndexSpec, Vec<BsonValue>)>, CommandError> {
    for (specification, entries_namespace) in indexes.iter().zip(namespaces) {
        for values in index_entries(document, specification) {
            let tuple = index_tuple(&values);
            let own = index_entry_key(&values, key);
            let probe = store
                .scan_reserved(&entries_namespace, &tuple)
                .map_err(|error| CommandError::new(1, format!("index probe failed: {error}")))?;
            if probe.iter().any(|(entry_key, _)| entry_key.as_slice() != own.as_slice()) {
                return Ok(Some((specification, values)));
            }
        }
    }
    Ok(None)
}

/// Reply shape shared by `insert` / `update` / `delete` / `count`, matching
/// `mongod 8.0.9`: `ok` is a double and every counter is int32.
///
/// The field order is also the real server's: `n`, `writeErrors`, `nModified`,
/// `ok`. A successful update therefore stays `n`, `nModified`, `ok`.
pub(crate) fn write_reply(count: i64, modified: Option<i64>, errors: &[WriteError]) -> BsonDocument {
    let mut response = BsonDocument::new();
    response.push("n".to_string(), count_value(count));
    if !errors.is_empty() {
        let entries: Vec<BsonValue> = errors
            .iter()
            .map(|error| {
                let mut entry = BsonDocument::new();
                entry.push("index".to_string(), BsonValue::Int32(error.index as i32));
                entry.push("code".to_string(), BsonValue::Int32(error.code));
                entry.push("errmsg".to_string(), BsonValue::String(error.message.clone()));
                if let Some(pattern) = &error.key_pattern {
                    entry.push("keyPattern".to_string(), BsonValue::Document(pattern.clone()));
                }
                if let Some(value) = &error.key_value {
                    entry.push("keyValue".to_string(), BsonValue::Document(value.clone()));
                }
                if let Some(info) = &error.err_info {
                    entry.push("errInfo".to_string(), BsonValue::Document(info.clone()));
                }
                BsonValue::Document(entry)
            })
            .collect();
        response.push("writeErrors".to_string(), BsonValue::Array(entries));
    }
    if let Some(modified) = modified {
        response.push("nModified".to_string(), count_value(modified));
    }
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response
}

pub(crate) fn validate_update_document(document: &BsonDocument) -> Result<(), CommandError> {
    if document.fields.is_empty() {
        return Err(CommandError::new(2, "update specification is empty"));
    }
    let mut paths: Vec<&str> = Vec::new();
    for (operator_index, (operator, operand)) in document.fields.iter().enumerate() {
        if !operator.starts_with('$') {
            return Err(CommandError::new(59, "replacement updates are not supported; `u` must use an update operator"));
        }
        if !UPDATE_OPERATORS.contains(&operator.as_str()) {
            return Err(CommandError::new(9, format!("Unknown modifier: {operator} (operatorIndex: {operator_index})")));
        }
        let BsonValue::Document(operand) = operand
        else {
            return Err(CommandError::new(9, format!("update operator {operator} requires a document")));
        };
        for (path, value) in &operand.fields {
            if path == "_id" || path.starts_with("_id.") {
                return Err(CommandError::new(66, format!("Performing an update on the path '{path}' would modify the immutable field '_id'")));
            }
            if paths.contains(&path.as_str()) {
                return Err(CommandError::new(40, format!("Updating the path '{path}' would create a conflict at '{path}'")));
            }
            paths.push(path.as_str());
            if operator == "$rename" {
                let BsonValue::String(target) = value
                else {
                    return Err(CommandError::new(2, format!("The 'to' field for $rename must be a string: {path}")));
                };
                if target == "_id" || target.starts_with("_id.") {
                    return Err(CommandError::new(
                        66,
                        format!("Performing an update on the path '{target}' would modify the immutable field '_id'"),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn apply_update(
    document: &BsonDocument,
    specification: &BsonDocument,
    identifier: &str,
    filter: &BsonDocument,
    on_insert: bool,
) -> Result<BsonDocument, CommandError> {
    let mut updated = document.clone();
    for (operator, operand) in &specification.fields {
        let BsonValue::Document(operand) = operand
        else {
            continue;
        };
        if matches!(operator.as_str(), "$set" | "$inc" | "$mul" | "$min" | "$max") {
            for path in operand.fields.iter().map(|(path, _)| path) {
                let segments = resolve_segments(path, document, filter)?;
                let path_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
                validate_negative_index(document, &path_refs)?;
            }
        }
        for (path, value) in &operand.fields {
            let segments = resolve_segments(path, &updated, filter)?;
            let path_refs: Vec<&str> = segments.iter().map(String::as_str).collect();
            match operator.as_str() {
                "$set" => set_segments(&mut updated, &path_refs, value.clone())?,
                "$unset" => unset_segments(&mut updated, &path_refs),
                "$inc" => numeric_segments(&mut updated, &path_refs, value, path, identifier, false)?,
                "$mul" => numeric_segments(&mut updated, &path_refs, value, path, identifier, true)?,
                // `$min` / `$max` keep the smaller / larger value under the BSON
                // ordering, and write the operand when the field is missing.
                "$min" => keep_extreme(&mut updated, &path_refs, value, true)?,
                "$max" => keep_extreme(&mut updated, &path_refs, value, false)?,
                // `$setOnInsert` is a no-op on a matched document and applies only
                // while constructing the document an upsert inserts.
                "$setOnInsert" => {
                    if on_insert {
                        set_segments(&mut updated, &path_refs, value.clone())?;
                    }
                }
                "$rename" => rename_path(&mut updated, &path_refs, value, path)?,
                "$push" => push_path(&mut updated, &path_refs, value, path, identifier)?,
                "$pop" => pop_path(&mut updated, &path_refs, value, path)?,
                "$addToSet" => add_to_set_path(&mut updated, &path_refs, value, path, identifier)?,
                "$pull" => pull_path(&mut updated, &path_refs, value, path)?,
                _ => {}
            }
        }
    }
    Ok(updated)
}

/// Resolves the positional `$` segment against the document and filter that
/// produced the match. Only the first matching array element is supported.
pub(crate) fn resolve_segments(path: &str, document: &BsonDocument, filter: &BsonDocument) -> Result<Vec<String>, CommandError> {
    let segments: Vec<String> = path.split('.').map(str::to_string).collect();
    if !segments.iter().any(|segment| segment == "$") {
        return Ok(segments);
    }
    let mut resolved = Vec::with_capacity(segments.len());
    for (position, segment) in segments.iter().enumerate() {
        if segment == "$" {
            let index = positional_index(document, &resolved, filter);
            resolved.push(index.to_string());
        }
        else {
            resolved.push(segment.clone());
        }
        let _ = position;
    }
    Ok(resolved)
}

/// Index of the first array element selected by the filter clause for the array
/// that precedes the positional operator.
pub(crate) fn positional_index(document: &BsonDocument, prefix: &[String], filter: &BsonDocument) -> usize {
    let prefix_refs: Vec<&str> = prefix.iter().map(String::as_str).collect();
    let Some(BsonValue::Array(items)) = value_at(document, &prefix_refs).cloned()
    else {
        return 0;
    };
    let key = prefix.join(".");
    if let Some(predicate) = filter.get(&key) {
        if let BsonValue::Document(operators) = predicate
            && has_operator(operators)
        {
            if let Some(BsonValue::Document(specification)) = operators.get("$elemMatch") {
                for (index, item) in items.iter().enumerate() {
                    if let BsonValue::Document(element) = item
                        && document_matches(element, specification).unwrap_or(false)
                    {
                        return index;
                    }
                }
            }
            else {
                for (index, item) in items.iter().enumerate() {
                    if operator_matches(&[item], operators, &key, 0, &AggregationExpressions).unwrap_or(false) {
                        return index;
                    }
                }
            }
            return 0;
        }
        for (index, item) in items.iter().enumerate() {
            if values_equal(Some(item), predicate) {
                return index;
            }
        }
    }
    let mut clauses = BsonDocument::new();
    let mut has_clause = false;
    for (field, expected) in &filter.fields {
        if let Some(rest) = field.strip_prefix(&format!("{key}.")) {
            clauses.push(rest.to_string(), expected.clone());
            has_clause = true;
        }
    }
    if has_clause {
        for (index, item) in items.iter().enumerate() {
            if let BsonValue::Document(element) = item
                && document_matches(element, &clauses).unwrap_or(false)
            {
                return index;
            }
        }
    }
    0
}

pub(crate) fn validate_negative_index(document: &BsonDocument, segments: &[&str]) -> Result<(), CommandError> {
    for position in 1..segments.len() {
        let Some(BsonValue::Array(items)) = value_at(document, &segments[..position])
        else {
            continue;
        };
        let Ok(number) = segments[position].parse::<i64>()
        else {
            continue;
        };
        if number < 0 {
            return Err(cannot_create_field_in_array(items, segments[position - 1], segments[position]));
        }
    }
    Ok(())
}

/// Every value reachable by `segments`, traversing arrays field-wise so
/// `a.b` over `[{b: 1}, {b: 2}]` yields both values.
pub(crate) fn collect_values_at<'a>(document: &'a BsonDocument, segments: &[&str], out: &mut Vec<&'a BsonValue>) {
    let Some((head, rest)) = segments.split_first()
    else {
        return;
    };
    if let Some(value) = document.get(head) {
        collect_descend_values(value, rest, out);
    }
}

pub(crate) fn collect_descend_values<'a>(value: &'a BsonValue, segments: &[&str], out: &mut Vec<&'a BsonValue>) {
    let Some((head, rest)) = segments.split_first()
    else {
        out.push(value);
        return;
    };
    match value {
        BsonValue::Document(document) => {
            if let Some(next) = document.get(head) {
                collect_descend_values(next, rest, out);
            }
        }
        BsonValue::Array(items) => {
            if let Ok(index) = head.parse::<usize>() {
                if let Some(next) = items.get(index) {
                    collect_descend_values(next, rest, out);
                }
            }
            else {
                for item in items {
                    collect_descend_values(item, segments, out);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn value_at<'a>(document: &'a BsonDocument, segments: &[&str]) -> Option<&'a BsonValue> {
    let head = segments.first()?;
    let value = document.get(head)?;
    if segments.len() == 1 {
        return Some(value);
    }
    descend_value(value, &segments[1..])
}

pub(crate) fn descend_value<'a>(value: &'a BsonValue, segments: &[&str]) -> Option<&'a BsonValue> {
    if segments.is_empty() {
        return Some(value);
    }
    match value {
        BsonValue::Document(document) => document.get(segments[0]).and_then(|next| descend_value(next, &segments[1..])),
        BsonValue::Array(items) => {
            let index = segments[0].parse::<usize>().ok()?;
            items.get(index).and_then(|next| descend_value(next, &segments[1..]))
        }
        _ => None,
    }
}

pub(crate) fn value_at_mut<'a>(document: &'a mut BsonDocument, segments: &[&str]) -> Option<&'a mut BsonValue> {
    let head = *segments.first()?;
    let position = document.fields.iter().position(|(field, _)| field == head)?;
    if segments.len() == 1 {
        return Some(&mut document.fields[position].1);
    }
    descend_value_mut(&mut document.fields[position].1, &segments[1..])
}

pub(crate) fn descend_value_mut<'a>(value: &'a mut BsonValue, segments: &[&str]) -> Option<&'a mut BsonValue> {
    if segments.is_empty() {
        return Some(value);
    }
    match value {
        BsonValue::Document(document) => {
            let position = document.fields.iter().position(|(field, _)| field == segments[0])?;
            descend_value_mut(&mut document.fields[position].1, &segments[1..])
        }
        BsonValue::Array(items) => {
            let index = segments[0].parse::<usize>().ok()?;
            let next = items.get_mut(index)?;
            descend_value_mut(next, &segments[1..])
        }
        _ => None,
    }
}

/// `$min` / `$max`: keep the operand only when it is smaller / larger than the
/// current value under the BSON ordering. A missing field always takes the
/// operand, matching `mongod` where `{ $min: { a: 1 } }` creates `a: 1`.
pub(crate) fn keep_extreme(document: &mut BsonDocument, segments: &[&str], value: &BsonValue, minimum: bool) -> Result<(), CommandError> {
    let replace = match value_at(document, segments) {
        None => true,
        Some(current) => {
            let ordering = compare_values(value, current);
            if minimum { ordering == Ordering::Less } else { ordering == Ordering::Greater }
        }
    };
    if replace {
        set_segments(document, segments, value.clone())?;
    }
    Ok(())
}

pub(crate) fn set_segments(document: &mut BsonDocument, segments: &[&str], value: BsonValue) -> Result<(), CommandError> {
    let head = segments[0];
    if segments.len() == 1 {
        if let Some(existing) = document.fields.iter_mut().find(|(field, _)| field == head) {
            existing.1 = value;
        }
        else {
            document.push(head.to_string(), value);
        }
        return Ok(());
    }
    let tail = &segments[1..];
    if !document.fields.iter().any(|(field, _)| field == head) {
        let mut child = BsonDocument::new();
        set_segments(&mut child, tail, value)?;
        document.push(head.to_string(), BsonValue::Document(child));
        return Ok(());
    }
    let position = document.fields.iter().position(|(field, _)| field == head).expect("head exists");
    match &mut document.fields[position].1 {
        BsonValue::Document(child) => set_segments(child, tail, value),
        BsonValue::Array(items) => set_array_segments(items, head, tail, value),
        other => {
            let rendered = render_shell_value(other);
            Err(CommandError::new(28, format!("Cannot create field '{}' in element {{{head}: {rendered}}}", tail[0])))
        }
    }
}

/// `$addFields` / `$set`: like `set_segments`, but a scalar along the path is
/// replaced by an embedded document (aggregation semantics), while arrays keep
/// the update-style error.
pub(crate) fn set_aggregation_segments(document: &mut BsonDocument, segments: &[&str], value: BsonValue) -> Result<(), CommandError> {
    let head = segments[0];
    if segments.len() == 1 {
        if let Some(existing) = document.fields.iter_mut().find(|(field, _)| field == head) {
            existing.1 = value;
        }
        else {
            document.push(head.to_string(), value);
        }
        return Ok(());
    }
    let tail = &segments[1..];
    let Some(position) = document.fields.iter().position(|(field, _)| field == head)
    else {
        let mut child = BsonDocument::new();
        set_aggregation_segments(&mut child, tail, value)?;
        document.push(head.to_string(), BsonValue::Document(child));
        return Ok(());
    };
    if matches!(&document.fields[position].1, BsonValue::Array(_)) {
        let rendered = render_shell_value(&document.fields[position].1);
        return Err(CommandError::new(28, format!("Cannot create field '{}' in element {{{head}: {rendered}}}", tail[0])));
    }
    match &mut document.fields[position].1 {
        BsonValue::Document(child) => set_aggregation_segments(child, tail, value),
        _ => {
            let mut child = BsonDocument::new();
            set_aggregation_segments(&mut child, tail, value)?;
            document.fields[position].1 = BsonValue::Document(child);
            Ok(())
        }
    }
}

pub(crate) fn set_array_segments(items: &mut Vec<BsonValue>, field: &str, segments: &[&str], value: BsonValue) -> Result<(), CommandError> {
    let part = segments[0];
    let Ok(number) = part.parse::<i64>()
    else {
        return Err(cannot_create_field_in_array(items, field, part));
    };
    if number < 0 {
        return Err(cannot_create_field_in_array(items, field, part));
    }
    let index = number as usize;
    if index >= items.len() {
        items.resize(index + 1, BsonValue::Null);
    }
    if segments.len() == 1 {
        items[index] = value;
        return Ok(());
    }
    if matches!(items[index], BsonValue::Null) {
        items[index] = BsonValue::Document(BsonDocument::new());
    }
    match &mut items[index] {
        BsonValue::Document(child) => set_segments(child, &segments[1..], value),
        BsonValue::Array(inner) => set_array_segments(inner, part, &segments[1..], value),
        other => {
            let rendered = render_shell_value(other);
            Err(CommandError::new(28, format!("Cannot create field '{}' in element {rendered}", segments[1])))
        }
    }
}

pub(crate) fn cannot_create_field_in_array(items: &[BsonValue], field: &str, child: &str) -> CommandError {
    let rendered = render_shell_value(&BsonValue::Array(items.to_vec()));
    CommandError::new(28, format!("Cannot create field '{child}' in element {{{field}: {rendered}}}"))
}

pub(crate) fn unset_segments(document: &mut BsonDocument, segments: &[&str]) {
    let head = segments[0];
    if segments.len() == 1 {
        document.fields.retain(|(field, _)| field != head);
        return;
    }
    let position = match document.fields.iter().position(|(field, _)| field == head) {
        Some(position) => position,
        None => return,
    };
    match &mut document.fields[position].1 {
        BsonValue::Document(child) => unset_segments(child, &segments[1..]),
        BsonValue::Array(items) => unset_array_segments(items, &segments[1..]),
        _ => {}
    }
}

pub(crate) fn unset_array_segments(items: &mut [BsonValue], segments: &[&str]) {
    let Ok(index) = segments[0].parse::<usize>()
    else {
        return;
    };
    if index >= items.len() {
        return;
    }
    if segments.len() == 1 {
        items[index] = BsonValue::Null;
        return;
    }
    match &mut items[index] {
        BsonValue::Document(child) => unset_segments(child, &segments[1..]),
        BsonValue::Array(inner) => unset_array_segments(inner, &segments[1..]),
        _ => {}
    }
}

pub(crate) fn numeric_segments(
    document: &mut BsonDocument,
    segments: &[&str],
    amount: &BsonValue,
    path: &str,
    identifier: &str,
    multiply: bool,
) -> Result<(), CommandError> {
    let current = value_at(document, segments).cloned();
    let result = match current {
        Some(value) => {
            if multiply {
                multiply_value(&value, amount).ok_or_else(|| {
                    CommandError::new(
                        14,
                        format!(
                            "Cannot apply $mul to a value of non-numeric type. {{_id: {identifier}}} has the field '{path}' of non-numeric type {}",
                            bson_type_name(&value)
                        ),
                    )
                })?
            }
            else {
                increment_value(&value, amount).ok_or_else(|| {
                    CommandError::new(14, format!("Cannot apply $inc to a value of non-numeric type (field: \"{path}\", _id: {identifier})"))
                })?
            }
        }
        None => {
            if multiply {
                zero_of(amount)
            }
            else {
                if number_of(amount).is_none() {
                    return Err(CommandError::new(
                        14,
                        format!("Cannot apply $inc to a value of non-numeric type (field: \"{path}\", _id: {identifier})"),
                    ));
                }
                amount.clone()
            }
        }
    };
    set_segments(document, segments, result)
}

pub(crate) fn rename_path(document: &mut BsonDocument, source: &[&str], value: &BsonValue, path: &str) -> Result<(), CommandError> {
    let BsonValue::String(target) = value
    else {
        return Ok(());
    };
    if path == target {
        return Err(CommandError::new(2, format!("The source and target field for $rename must differ: {path}: \"{target}\"")));
    }
    let Some(source_value) = take_path(document, source)
    else {
        return Ok(());
    };
    let target_segments: Vec<&str> = target.split('.').collect();
    unset_segments(document, &target_segments);
    set_segments(document, &target_segments, source_value)
}

pub(crate) fn take_path(document: &mut BsonDocument, segments: &[&str]) -> Option<BsonValue> {
    let head = *segments.first()?;
    let position = document.fields.iter().position(|(field, _)| field == head)?;
    if segments.len() == 1 {
        return Some(document.fields.remove(position).1);
    }
    match &mut document.fields[position].1 {
        BsonValue::Document(child) => take_path(child, &segments[1..]),
        _ => None,
    }
}

pub(crate) fn push_path(
    document: &mut BsonDocument,
    segments: &[&str],
    value: &BsonValue,
    path: &str,
    identifier: &str,
) -> Result<(), CommandError> {
    let items = ensure_array(document, segments, path, identifier)?;
    if let BsonValue::Document(modifiers) = value
        && has_operator(modifiers)
    {
        let values = match modifiers.get("$each") {
            Some(BsonValue::Array(values)) => values.clone(),
            Some(_) => return Err(CommandError::new(2, "$each requires an array")),
            None => return Err(CommandError::new(2, "$slice and $position require $each")),
        };
        for (key, _) in &modifiers.fields {
            if !matches!(key.as_str(), "$each" | "$slice" | "$position") {
                return Err(CommandError::new(9, format!("Unrecognized clause in $push: {key}")));
            }
        }
        let position = match modifiers.get("$position") {
            Some(operand) => match integer_of(operand) {
                Some(position) => {
                    if position < 0 {
                        let from_end = items.len() as i128 + position;
                        Some(from_end.max(0) as usize)
                    }
                    else {
                        Some((position as usize).min(items.len()))
                    }
                }
                None => return Err(CommandError::new(2, "$position must be an integer")),
            },
            None => None,
        };
        match position {
            Some(position) => {
                let tail = items.split_off(position);
                items.extend(values);
                items.extend(tail);
            }
            None => items.extend(values),
        }
        if let Some(operand) = modifiers.get("$slice") {
            let Some(slice) = integer_of(operand)
            else {
                return Err(CommandError::new(2, "$slice must be an integer"));
            };
            if slice >= 0 {
                items.truncate(slice as usize);
            }
            else {
                let keep = (-slice) as usize;
                if items.len() > keep {
                    let drop = items.len() - keep;
                    items.drain(0..drop);
                }
            }
        }
        return Ok(());
    }
    items.push(value.clone());
    Ok(())
}

pub(crate) fn add_to_set_path(
    document: &mut BsonDocument,
    segments: &[&str],
    value: &BsonValue,
    path: &str,
    identifier: &str,
) -> Result<(), CommandError> {
    ensure_array_with_message(document, segments, path, identifier, |field, kind| {
        format!("Cannot apply $addToSet to non-array field. Field named '{field}' has non-array type {kind}")
    })
    .and_then(|items| {
        if let BsonValue::Document(modifiers) = value
            && has_operator(modifiers)
        {
            for (key, _) in &modifiers.fields {
                if key != "$each" {
                    return Err(CommandError::new(9, format!("Found unexpected fields after $each in $addToSet: {key}")));
                }
            }
            let Some(BsonValue::Array(values)) = modifiers.get("$each")
            else {
                return Err(CommandError::new(2, "$each requires an array"));
            };
            for item in values {
                if !items.iter().any(|existing| values_equal(Some(existing), item)) {
                    items.push(item.clone());
                }
            }
            return Ok(());
        }
        if !items.iter().any(|existing| values_equal(Some(existing), value)) {
            items.push(value.clone());
        }
        Ok(())
    })
}

pub(crate) fn pop_path(document: &mut BsonDocument, segments: &[&str], value: &BsonValue, path: &str) -> Result<(), CommandError> {
    let direction = match value {
        BsonValue::Int32(number) => *number,
        BsonValue::Int64(number) => *number as i32,
        _ => 0,
    };
    if direction != 1 && direction != -1 {
        return Err(CommandError::new(9, format!("$pop expects 1 or -1, found: {}", render_shell_value(value))));
    }
    let Some(current) = value_at(document, segments).cloned()
    else {
        return Ok(());
    };
    let BsonValue::Array(items) = current
    else {
        return Err(CommandError::new(14, format!("Path '{path}' contains an element of non-array type '{}'", bson_type_name(&current))));
    };
    if items.is_empty() {
        return Ok(());
    }
    let mut items = items;
    if direction == 1 {
        items.pop();
    }
    else {
        items.remove(0);
    }
    set_segments(document, segments, BsonValue::Array(items))
}

pub(crate) fn pull_path(document: &mut BsonDocument, segments: &[&str], value: &BsonValue, path: &str) -> Result<(), CommandError> {
    let Some(current) = value_at(document, segments).cloned()
    else {
        return Ok(());
    };
    let BsonValue::Array(items) = current
    else {
        return Err(CommandError::new(2, "Cannot apply $pull to a non-array value"));
    };
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        if !pull_matches(&item, value, path)? {
            kept.push(item);
        }
    }
    set_segments(document, segments, BsonValue::Array(kept))
}

pub(crate) fn pull_matches(item: &BsonValue, operand: &BsonValue, _path: &str) -> Result<bool, CommandError> {
    if let BsonValue::Document(operand) = operand {
        if has_operator(operand) {
            return operator_matches(&[item], operand, "pull", 0, &AggregationExpressions);
        }
        return match item {
            BsonValue::Document(element) => document_matches(element, operand),
            _ => Ok(false),
        };
    }
    Ok(values_equal(Some(item), operand))
}

pub(crate) fn ensure_array<'a>(
    document: &'a mut BsonDocument,
    segments: &[&str],
    path: &str,
    identifier: &str,
) -> Result<&'a mut Vec<BsonValue>, CommandError> {
    ensure_array_with_message(document, segments, path, identifier, |field, kind| {
        format!("The field '{field}' must be an array but is of type {kind} in document {{_id: {identifier}}}")
    })
}

pub(crate) fn ensure_array_with_message<'a>(
    document: &'a mut BsonDocument,
    segments: &[&str],
    path: &str,
    identifier: &str,
    message: impl Fn(&str, &str) -> String,
) -> Result<&'a mut Vec<BsonValue>, CommandError> {
    let _ = identifier;
    if value_at(document, segments).is_none() {
        set_segments(document, segments, BsonValue::Array(Vec::new()))?;
    }
    let value = value_at_mut(document, segments).expect("array path was created");
    match value {
        BsonValue::Array(items) => Ok(items),
        other => {
            let kind = bson_type_name(other);
            Err(CommandError::new(2, message(path, kind)))
        }
    }
}

pub(crate) fn zero_of(amount: &BsonValue) -> BsonValue {
    match amount {
        BsonValue::Int32(_) => BsonValue::Int32(0),
        BsonValue::Int64(_) => BsonValue::Int64(0),
        BsonValue::Double(_) => BsonValue::Double(0.0),
        other => other.clone(),
    }
}

pub(crate) fn multiply_value(current: &BsonValue, amount: &BsonValue) -> Option<BsonValue> {
    let left = number_of(current)?;
    let right = number_of(amount)?;
    let product = left * right;
    if matches!(current, BsonValue::Double(_)) || matches!(amount, BsonValue::Double(_)) {
        return Some(BsonValue::Double(product));
    }
    if matches!(current, BsonValue::Int64(_)) || matches!(amount, BsonValue::Int64(_)) {
        return Some(BsonValue::Int64(product as i64));
    }
    if let Ok(value) = i32::try_from(product as i64) { Some(BsonValue::Int32(value)) } else { Some(BsonValue::Int64(product as i64)) }
}

/// Whether a requested key pattern names the same index key as `specification`.
///
/// Field order is significant and each value uses the numeric-equivalence key
/// encoding, so `{ a: 1 }` matches `{ a: 1.0 }` but not `{ a: "1" }` and not
/// a reordered compound key.
pub(crate) fn key_pattern_matches(requested: &BsonDocument, specification: &BsonDocument) -> bool {
    requested.fields.len() == specification.fields.len()
        && requested
            .fields
            .iter()
            .zip(&specification.fields)
            .all(|((want, left), (have, right))| want == have && value_to_key(left) == value_to_key(right))
}

/// BSON type name `mongod` prints in a wrong-type error message.
pub(crate) fn bson_type_name(value: &BsonValue) -> &'static str {
    match value {
        BsonValue::Document(_) => "object",
        BsonValue::Array(_) => "array",
        BsonValue::String(_) => "string",
        BsonValue::Int32(_) => "int",
        BsonValue::Int64(_) => "long",
        BsonValue::Double(_) => "double",
        BsonValue::Bool(_) => "bool",
        BsonValue::Null => "null",
        BsonValue::Undefined => "undefined",
        BsonValue::ObjectId(_) => "objectId",
        BsonValue::Binary(_) => "binData",
        BsonValue::DateTime(_) => "date",
        BsonValue::Timestamp { .. } => "timestamp",
        BsonValue::Decimal128(_) => "decimal",
        BsonValue::Regex { .. } => "regex",
        BsonValue::Code(_) => "javascript",
        BsonValue::CodeWithScope { .. } => "javascriptWithScope",
        BsonValue::Symbol(_) => "symbol",
        BsonValue::DbPointer { .. } => "dbPointer",
        BsonValue::MinKey => "minKey",
        BsonValue::MaxKey => "maxKey",
    }
}

pub(crate) fn increment_value(current: &BsonValue, amount: &BsonValue) -> Option<BsonValue> {
    match (current, amount) {
        (BsonValue::Int32(left), BsonValue::Int32(right)) => {
            let sum = i128::from(*left) + i128::from(*right);
            if let Ok(value) = i32::try_from(sum) { Some(BsonValue::Int32(value)) } else { i64::try_from(sum).ok().map(BsonValue::Int64) }
        }
        (BsonValue::Int32(_) | BsonValue::Int64(_), BsonValue::Int32(_) | BsonValue::Int64(_)) => {
            let sum = integer_of(current)? + integer_of(amount)?;
            i64::try_from(sum).ok().map(BsonValue::Int64)
        }
        (BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_), BsonValue::Double(_))
        | (BsonValue::Double(_), BsonValue::Int32(_) | BsonValue::Int64(_)) => {
            Some(BsonValue::Double(number_of(current)? + number_of(amount)?))
        }
        _ => None,
    }
}

pub(crate) fn index_entry_key(values: &[BsonValue], document_key: &[u8]) -> Vec<u8> {
    let mut key = index_tuple(values);
    key.extend_from_slice(document_key);
    key
}

/// 一个文档在一组索引下应当存在的全部条目。
pub(crate) fn document_index_entries(
    document: &BsonDocument,
    document_key: &[u8],
    specifications: &[IndexSpec],
) -> BTreeSet<(String, Vec<u8>)> {
    let mut entries = BTreeSet::new();
    for specification in specifications {
        let namespace = crate::mongo::storage::index_namespace(&specification.namespace, &specification.name);
        for values in index_entries(document, specification) {
            entries.insert((namespace.clone(), index_entry_key(&values, document_key)));
        }
    }
    entries
}

/// 按命名空间写入索引项；事务内与文档写一样走暂存，提交时一起落地。
pub(crate) fn put_index_keys(
    store: &dyn MongoStorage,
    session: Option<&str>,
    entries: &BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>,
) -> Result<(), CommandError> {
    for (namespace, entries) in entries {
        match session {
            Some(session) => {
                for (entry_key, value) in entries {
                    if store.transaction_write(session, namespace, entry_key, Some(value.clone())) == TransactionWrite::Conflict {
                        return Err(write_conflict_error());
                    }
                }
            }
            None => {
                store.put_reserved(namespace, entries).map_err(|error| CommandError::new(2, format!("index write failed: {error}")))?;
            }
        }
    }
    Ok(())
}

/// 按命名空间删除索引项；事务内暂存删除，提交时一起落地。
pub(crate) fn delete_index_keys(
    store: &dyn MongoStorage,
    session: Option<&str>,
    keys: &BTreeMap<String, Vec<Vec<u8>>>,
) -> Result<(), CommandError> {
    for (namespace, keys) in keys {
        if keys.is_empty() {
            continue;
        }
        match session {
            Some(session) => {
                for key in keys {
                    if store.transaction_write(session, namespace, key, None) == TransactionWrite::Conflict {
                        return Err(write_conflict_error());
                    }
                }
            }
            None => {
                store.delete(namespace, keys).map_err(|error| CommandError::new(2, format!("index delete failed: {error}")))?;
            }
        }
    }
    Ok(())
}

/// 索引键字段出现数组值时把索引标记为 multikey，供范围计划保守回退。
///
/// 标记只会从关变开：数组元素删除后仍按 multikey 处理，宁可少用索引也不漏结果。
pub(crate) fn note_multikey(
    store: &dyn MongoStorage,
    documents: &[(BsonDocument, Vec<u8>)],
    specifications: &[IndexSpec],
) -> Result<(), CommandError> {
    for specification in specifications {
        if specification.multikey {
            continue;
        }
        let array_valued =
            documents.iter().any(|(document, _)| specification.key.fields.iter().any(|(field, _)| field_array_valued(document, field)));
        if array_valued {
            store
                .mark_index_multikey(&specification.namespace, &specification.name)
                .map_err(|error| CommandError::new(1, format!("index metadata update failed: {error}")))?;
        }
    }
    Ok(())
}

/// 一个文档集合的全部索引项，按索引项命名空间分组。
///
/// 与 [`write_index_entries`] 同一套键构造，只是把结果交回调用方，让写命令能把
/// 索引项与文档合成同一次提交。
pub(crate) fn index_entries_by_namespace(
    documents: &[(BsonDocument, Vec<u8>)],
    specifications: &[IndexSpec],
) -> BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>> {
    let mut by_namespace: BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>> = BTreeMap::new();
    if specifications.is_empty() || documents.is_empty() {
        return by_namespace;
    }
    for (document, document_key) in documents {
        for (namespace, entry_key) in document_index_entries(document, document_key, specifications) {
            by_namespace.entry(namespace).or_default().push((entry_key, document_key.clone()));
        }
    }
    by_namespace
}

/// 为一个文档集合写入索引项；每个条目值是文档键，读取时据此回表。
pub(crate) fn write_index_entries(
    store: &dyn MongoStorage,
    session: Option<&str>,
    documents: &[(BsonDocument, Vec<u8>)],
    specifications: &[IndexSpec],
) -> Result<(), CommandError> {
    if specifications.is_empty() || documents.is_empty() {
        return Ok(());
    }
    note_multikey(store, documents, specifications)?;
    let by_namespace = index_entries_by_namespace(documents, specifications);
    put_index_keys(store, session, &by_namespace)
}

/// 为 `mongodump` 导入的文档补写索引项；导入走覆盖写，调用方在写完后立即调用。
///
/// 覆盖写会替换掉同 `_id` 的旧文档，旧文档的索引项必须一并清掉，否则旧索引值
/// 会留下指向已变更文档的条目。先删旧条目再写新条目，删除期间读到的旧条目只
/// 会取到旧文档并被完整过滤条件拒绝，不会产生错误命中。
pub fn maintain_imported_indexes(store: &dyn MongoStorage, namespace: &str, documents: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
    let specifications = store.index_specs(namespace).map_err(|error| error.to_string())?;
    if specifications.is_empty() {
        return Ok(());
    }
    let parsed: Vec<(BsonDocument, Vec<u8>)> =
        documents.iter().filter_map(|(key, payload)| parse_bson(payload).ok().map(|document| (document, key.clone()))).collect();
    let mut overwritten: Vec<(BsonDocument, Vec<u8>)> = Vec::new();
    for (_, key) in &parsed {
        match store.get(namespace, key) {
            Ok(Some(payload)) => {
                if let Ok(document) = parse_bson(&payload) {
                    overwritten.push((document, key.clone()));
                }
            }
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    remove_index_entries(store, None, &overwritten, &specifications).map_err(|error| error.message)?;
    write_index_entries(store, None, &parsed, &specifications).map_err(|error| error.message)?;
    Ok(())
}

/// 删除一个文档集合的全部索引项。
pub(crate) fn remove_index_entries(
    store: &dyn MongoStorage,
    session: Option<&str>,
    documents: &[(BsonDocument, Vec<u8>)],
    specifications: &[IndexSpec],
) -> Result<(), CommandError> {
    if specifications.is_empty() || documents.is_empty() {
        return Ok(());
    }
    let mut by_namespace: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    for (document, document_key) in documents {
        for (namespace, entry_key) in document_index_entries(document, document_key, specifications) {
            by_namespace.entry(namespace).or_default().push(entry_key);
        }
    }
    delete_index_keys(store, session, &by_namespace)
}

/// Stages a batch of new documents in the session's transaction, or writes it
/// straight to the shard outside one.
///
/// `documents` pairs each accepted document with its encoded `_id` key, so the
/// index entries are derived from the document the caller already has instead of
/// parsing the payload back.
pub(crate) fn stage_insert(
    store: &dyn MongoStorage,
    session: Option<&str>,
    namespace: &str,
    documents: &[(BsonDocument, Vec<u8>)],
) -> Result<(), CommandError> {
    let specifications = store.index_specs(namespace)?;
    if let Some(key) = session {
        if !specifications.is_empty() {
            write_index_entries(store, Some(key), documents, &specifications)?;
        }
        for (document, document_key) in documents {
            let payload = encode_bson(document);
            if store.transaction_write(key, namespace, document_key, Some(payload)) == TransactionWrite::Conflict {
                return Err(write_conflict_error());
            }
        }
        return Ok(());
    }
    let payloads: Vec<(Vec<u8>, Vec<u8>)> = documents.iter().map(|(document, key)| (key.clone(), encode_bson(document))).collect();
    // 文档与索引项合成一次提交：同一条 `insert` 只付一次落盘，也不会出现
    // 「索引项已落、文档未落」的中间态。
    let mut index_writes: BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>> = BTreeMap::new();
    if !specifications.is_empty() {
        note_multikey(store, documents, &specifications)?;
        index_writes = index_entries_by_namespace(documents, &specifications);
    }
    let mut writes: Vec<NamespaceWrite<'_>> = Vec::with_capacity(index_writes.len() + 1);
    for (entry_namespace, entries) in &index_writes {
        writes.push(NamespaceWrite { namespace: entry_namespace, puts: entries, deletes: &[] });
    }
    writes.push(NamespaceWrite { namespace, puts: &payloads, deletes: &[] });
    store.commit_namespaces(&writes).map_err(|error| CommandError::new(2, format!("insert failed: {error}")))?;
    Ok(())
}

/// Stages replacements in the session's transaction, or writes them straight
/// to the shard outside one.
pub(crate) fn stage_replace(
    store: &dyn MongoStorage,
    session: Option<&str>,
    namespace: &str,
    documents: &[(Vec<u8>, Vec<u8>)],
) -> Result<usize, CommandError> {
    let specifications = store.index_specs(namespace)?;
    let mut removed: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    let mut added: BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>> = BTreeMap::new();
    if !specifications.is_empty() {
        for (document_key, payload) in documents {
            let Ok(replacement) = parse_bson(payload)
            else {
                continue;
            };
            // 替换出来的文档可能第一次把某个索引字段变成数组，标记后两侧范围退回集合扫描。
            if specifications.iter().any(|specification| {
                !specification.multikey && specification.key.fields.iter().any(|(field, _)| field_array_valued(&replacement, field))
            }) {
                note_multikey(store, &[(replacement.clone(), document_key.clone())], &specifications)?;
            }
            let current_keys = document_index_entries(&replacement, document_key, &specifications);
            let previous_keys = match namespace_document(store, namespace, document_key, session)? {
                Some(previous) => match parse_bson(&previous) {
                    Ok(previous) => document_index_entries(&previous, document_key, &specifications),
                    Err(_) => BTreeSet::new(),
                },
                None => BTreeSet::new(),
            };
            for (entry_namespace, entry_key) in current_keys.difference(&previous_keys) {
                added.entry(entry_namespace.clone()).or_default().push((entry_key.clone(), document_key.clone()));
            }
            for (entry_namespace, entry_key) in previous_keys.difference(&current_keys) {
                removed.entry(entry_namespace.clone()).or_default().push(entry_key.clone());
            }
        }
    }
    match session {
        Some(key) => {
            // 新增项先落地、旧项最后删除：中途失败只会多出可被完整过滤条件淘汰的条目。
            if !specifications.is_empty() {
                put_index_keys(store, Some(key), &added)?;
            }
            for (document_key, payload) in documents {
                if store.transaction_write(key, namespace, document_key, Some(payload.clone())) == TransactionWrite::Conflict {
                    return Err(write_conflict_error());
                }
            }
            if !specifications.is_empty() {
                delete_index_keys(store, Some(key), &removed)?;
            }
            Ok(documents.len())
        }
        None => {
            // 文档与索引项一次提交：一条 `update` 只付一次落盘。
            let mut writes: Vec<NamespaceWrite<'_>> = Vec::with_capacity(added.len() + removed.len() + 1);
            for (entry_namespace, entries) in &added {
                writes.push(NamespaceWrite { namespace: entry_namespace, puts: entries, deletes: &[] });
            }
            writes.push(NamespaceWrite { namespace, puts: documents, deletes: &[] });
            for (entry_namespace, keys) in &removed {
                writes.push(NamespaceWrite { namespace: entry_namespace, puts: &[], deletes: keys });
            }
            store.commit_namespaces(&writes).map_err(|error| CommandError::new(2, error.to_string()))?;
            Ok(documents.len())
        }
    }
}

/// Stages deletions in the session's transaction, or writes them straight to
/// the shard outside one.
pub(crate) fn stage_delete(store: &dyn MongoStorage, session: Option<&str>, namespace: &str, keys: &[Vec<u8>]) -> Result<usize, CommandError> {
    let specifications = store.index_specs(namespace)?;
    let mut removed: Vec<(BsonDocument, Vec<u8>)> = Vec::new();
    if !specifications.is_empty() {
        for key in keys {
            if let Some(payload) = namespace_document(store, namespace, key, session)?
                && let Ok(document) = parse_bson(&payload)
            {
                removed.push((document, key.clone()));
            }
        }
    }
    match session {
        Some(key) => {
            for document_key in keys {
                if store.transaction_write(key, namespace, document_key, None) == TransactionWrite::Conflict {
                    return Err(write_conflict_error());
                }
            }
            if !specifications.is_empty() {
                remove_index_entries(store, session, &removed, &specifications)?;
            }
            Ok(keys.len())
        }
        None => {
            // 文档与索引项一次提交：一条 `delete` 只付一次落盘。
            let mut index_deletes: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
            if !specifications.is_empty() {
                for (document, document_key) in &removed {
                    for (entry_namespace, entry_key) in document_index_entries(document, document_key, &specifications) {
                        index_deletes.entry(entry_namespace).or_default().push(entry_key);
                    }
                }
            }
            let mut writes: Vec<NamespaceWrite<'_>> = Vec::with_capacity(index_deletes.len() + 1);
            writes.push(NamespaceWrite { namespace, puts: &[], deletes: keys });
            for (entry_namespace, entry_keys) in &index_deletes {
                writes.push(NamespaceWrite { namespace: entry_namespace, puts: &[], deletes: entry_keys });
            }
            let applied = store.commit_namespaces(&writes).map_err(|error| CommandError::new(2, error.to_string()))?;
            Ok(applied[0])
        }
    }
}

/// The namespace as seen by the session: staged writes layered over committed
/// data when a transaction is open, committed data otherwise.
pub(crate) fn namespace_documents(
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
    if let Some(session) = session
        && let Some(documents) = store.transaction_scan(session, namespace)?
    {
        return Ok(documents);
    }
    Ok(store.scan(namespace)?)
}

/// One document as seen by the session, including staged inserts, updates, and
/// deletes.
pub(crate) fn namespace_document(
    store: &dyn MongoStorage,
    namespace: &str,
    key: &[u8],
    session: Option<&str>,
) -> Result<Option<Vec<u8>>, CommandError> {
    if let Some(session) = session
        && let Some(staged) = store.transaction_get(session, namespace, key)?
    {
        return Ok(staged);
    }
    Ok(store.get(namespace, key)?)
}

pub(crate) fn command_bool(command: &BsonDocument, field: &str, default: bool) -> bool {
    match command.get(field) {
        Some(BsonValue::Bool(flag)) => *flag,
        _ => default,
    }
}

pub(crate) fn find_projection(documents: Vec<BsonDocument>, fields: Option<&BsonValue>) -> Result<Vec<BsonDocument>, CommandError> {
    match fields {
        Some(BsonValue::Document(specification)) => project_stage(&documents, specification),
        _ => Ok(documents),
    }
}

/// `findAndModify`: `remove`, `new`, `upsert`, `fields`, and `sort`.
pub fn find_and_modify(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "findAndModify")
        .ok_or_else(|| CommandError::new(73, "findAndModify command missing collection name"))?;
    reject_transaction_write_concern(command)?;
    let session = SessionContext::new(command, store)?;
    let empty = BsonDocument::new();
    let filter = match command.get("query") {
        Some(BsonValue::Document(filter)) => filter,
        _ => &empty,
    };
    validate_filter(filter, "findAndModify")?;
    let remove = command_bool(command, "remove", false);
    let return_new = command_bool(command, "new", false);
    let upsert = command_bool(command, "upsert", false);
    let update = match command.get("update") {
        Some(BsonValue::Document(update)) => Some(update),
        _ => None,
    };
    if remove && update.is_some() {
        return Err(CommandError::new(9, "Cannot specify both an update and remove=true".to_string()));
    }
    if !remove && update.is_none() {
        return Err(CommandError::new(9, "Either an update or remove=true must be specified".to_string()));
    }
    if remove && return_new {
        return Err(CommandError::new(
            9,
            "Cannot specify both new=true and remove=true; 'remove' always returns the deleted document".to_string(),
        ));
    }

    let sort = parse_sort(command.get("sort"));
    // 无排序时只需要第一篇命中；有排序时顺序由排序决定，仍要读完整段再排。
    let mut documents = if sort.is_empty() {
        collect_matching_bounded(store, &namespace, filter, session.key(), false, Some(1))?.0
    }
    else {
        collect_matching(store, &namespace, filter, session.key())?
    };
    sort_documents(&mut documents, &sort);
    let matched = documents.first().cloned();

    if remove {
        let mut last_error = BsonDocument::new();
        if let Some(document) = &matched {
            last_error.push("n".to_string(), BsonValue::Int32(1));
            if let Some(identifier) = document.get("_id") {
                stage_delete(store, session.key(), &namespace, &[value_to_key(identifier)])?;
                if session.key().is_none() && store.oplog_enabled() {
                    store.stage_oplog(OplogEntry::delete(&namespace, collection_uuid(&namespace), identifier));
                }
            }
            let value = find_projection(vec![document.clone()], command.get("fields"))?;
            return Ok(find_and_modify_reply(last_error, value.into_iter().next()));
        }
        last_error.push("n".to_string(), BsonValue::Int32(0));
        return Ok(find_and_modify_reply(last_error, None));
    }

    let update_document = update.expect("update checked above");
    let operator_update = update_document.fields.iter().any(|(field, _)| field.starts_with('$'));
    let index_specs = store.index_specs(&namespace)?;
    if let Some(document) = &matched {
        let identifier_value = document.get("_id").cloned();
        let identifier = identifier_value.as_ref().map(describe_id).unwrap_or_else(|| "<missing>".to_string());
        let updated = if operator_update {
            apply_update(document, update_document, &identifier, filter, false)?
        }
        else {
            build_replacement(update_document, identifier_value.as_ref())?
        };
        if let Some(info) = update_validation_failure(store, &namespace, document, &updated)? {
            return Ok(find_and_modify_validation_failure(info));
        }
        if let Some(error) = parallel_array_error(&updated, &index_specs) {
            return Err(CommandError::new(error.code, error.message));
        }
        let unique_indexes: Vec<IndexSpec> = index_specs.iter().filter(|index| index.unique).cloned().collect();
        if !unique_indexes.is_empty() {
            let key = value_to_key(identifier_value.as_ref().unwrap_or(&BsonValue::Null));
            // 与 `update` 同一条路径：唯一索引项已落盘且不在事务里时，按前缀探针
            // 检查冲突，不再为一次 findAndModify 整表建唯一键 map。
            let probe_namespaces: Option<Vec<String>> = if session.key().is_some() {
                None
            }
            else {
                let mut namespaces = Vec::with_capacity(unique_indexes.len());
                let mut ready = true;
                for specification in &unique_indexes {
                    let entries_namespace = crate::mongo::storage::index_namespace(&namespace, &specification.name);
                    let entries = store
                        .reserved_has_entries(&entries_namespace)
                        .map_err(|error| CommandError::new(1, format!("index probe failed: {error}")))?;
                    if !entries {
                        ready = false;
                        break;
                    }
                    namespaces.push(entries_namespace);
                }
                ready.then_some(namespaces)
            };
            let conflict = match &probe_namespaces {
                Some(namespaces) => unique_conflict_by_probe(store, namespaces, &unique_indexes, &updated, &key)?,
                None => {
                    let mut indexed: BTreeMap<Vec<u8>, Vec<BTreeSet<Vec<u8>>>> = BTreeMap::new();
                    for (other_key, payload) in namespace_documents(store, &namespace, session.key())? {
                        if let Ok(candidate) = parse_bson(&payload) {
                            indexed.insert(other_key, unique_indexes.iter().map(|spec| tuples_of(&candidate, spec)).collect());
                        }
                    }
                    find_unique_conflict(&unique_indexes, &updated, &key, &indexed)
                }
            };
            if let Some((specification, values)) = conflict {
                let failure = duplicate_index_error(0, &namespace, specification, &values);
                let mut error =
                    CommandError::new(11000, format!("Plan executor error during findAndModify :: caused by :: {}", failure.message));
                if let Some(pattern) = failure.key_pattern {
                    error = error.with_detail("keyPattern", BsonValue::Document(pattern));
                }
                if let Some(value) = failure.key_value {
                    error = error.with_detail("keyValue", BsonValue::Document(value));
                }
                return Err(error);
            }
        }
        let key = value_to_key(identifier_value.as_ref().unwrap_or(&BsonValue::Null));
        stage_replace(store, session.key(), &namespace, &[(key, encode_bson(&updated))])?;
        stage_update_oplog(store, session.key(), &namespace, &[(document.clone(), updated.clone())]);
        let mut last_error = BsonDocument::new();
        last_error.push("n".to_string(), BsonValue::Int32(1));
        last_error.push("updatedExisting".to_string(), BsonValue::Bool(true));
        let selected = if return_new { updated } else { document.clone() };
        let value = find_projection(vec![selected], command.get("fields"))?;
        return Ok(find_and_modify_reply(last_error, value.into_iter().next()));
    }

    if upsert {
        let mut inserted = if operator_update {
            let base = upsert_base(filter);
            let identifier = base.get("_id").map(describe_id).unwrap_or_else(|| "<missing>".to_string());
            apply_update(&base, update_document, &identifier, filter, true)?
        }
        else {
            let query_identifier = match filter.get("_id") {
                Some(BsonValue::Document(operators)) if has_operator(operators) => None,
                Some(value) => Some(value.clone()),
                None => None,
            };
            build_replacement(update_document, query_identifier.as_ref())?
        };
        if inserted.get("_id").is_none() {
            inserted.fields.insert(0, ("_id".to_string(), BsonValue::ObjectId(generate_object_id())));
        }
        if let Some(info) = insert_validation_failure(store, &namespace, &inserted)? {
            return Ok(find_and_modify_validation_failure(info));
        }
        let identifier = inserted.get("_id").cloned().unwrap_or(BsonValue::Null);
        let candidates = vec![(0_usize, inserted.clone(), identifier.clone())];
        let mut write_errors = Vec::new();
        let (inserted_count, accepted) = commit_insert(store, session.key(), &namespace, &candidates, &Vec::new(), true, &mut write_errors)?;
        if let Some(error) = write_errors.first() {
            return Err(CommandError::new(error.code, error.message.clone()));
        }
        if inserted_count == 1 {
            stage_insert_oplog(store, &namespace, &accepted);
        }
        let mut last_error = BsonDocument::new();
        last_error.push("n".to_string(), BsonValue::Int32(1));
        last_error.push("updatedExisting".to_string(), BsonValue::Bool(false));
        last_error.push("upserted".to_string(), identifier);
        let selected = if return_new { Some(inserted) } else { None };
        let value = match selected {
            Some(document) => find_projection(vec![document], command.get("fields"))?.into_iter().next(),
            None => None,
        };
        return Ok(find_and_modify_reply(last_error, value));
    }

    let mut last_error = BsonDocument::new();
    last_error.push("n".to_string(), BsonValue::Int32(0));
    last_error.push("updatedExisting".to_string(), BsonValue::Bool(false));
    Ok(find_and_modify_reply(last_error, None))
}

pub(crate) fn find_and_modify_reply(last_error: BsonDocument, value: Option<BsonDocument>) -> BsonDocument {
    let mut response = BsonDocument::new();
    response.push("lastErrorObject".to_string(), BsonValue::Document(last_error));
    response.push("value".to_string(), value.map(BsonValue::Document).unwrap_or(BsonValue::Null));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response
}

pub(crate) fn find_and_modify_validation_failure(info: BsonDocument) -> BsonDocument {
    let mut last_error = BsonDocument::new();
    last_error.push("n".to_string(), BsonValue::Int32(0));
    last_error.push("updatedExisting".to_string(), BsonValue::Bool(false));
    let mut response = BsonDocument::new();
    response.push("lastErrorObject".to_string(), BsonValue::Document(last_error));
    response.push("value".to_string(), BsonValue::Null);
    response.push("code".to_string(), BsonValue::Int32(121));
    response.push("errmsg".to_string(), BsonValue::String("Document failed validation".to_string()));
    response.push("errInfo".to_string(), BsonValue::Document(info));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    response
}

/// `bulkWrite` over `admin`, with ordered / unordered execution and the
/// aggregate counters the server reports alongside per-operation results.
pub fn bulk_write(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    if command.database() != "admin" {
        return Err(CommandError::new(13, "bulkWrite may only be run against the admin database.".to_string()));
    }
    let Some(BsonValue::Array(operations)) = command.get("ops")
    else {
        return Err(CommandError::new(40414, "BSON field 'bulkWrite.ops' is missing but a required field"));
    };
    if operations.is_empty() || operations.len() > 100_000 {
        return Err(CommandError::new(16, format!("Write batch sizes must be between 1 and 100000. Got {} operations.", operations.len())));
    }
    let Some(BsonValue::Array(namespaces)) = command.get("nsInfo")
    else {
        return Err(CommandError::new(40414, "BSON field 'bulkWrite.nsInfo' is missing but a required field"));
    };
    let names: Vec<String> = namespaces
        .iter()
        .map(|entry| match entry {
            BsonValue::Document(entry) => match entry.get("ns") {
                Some(BsonValue::String(name)) => Ok(name.clone()),
                _ => Err(CommandError::new(2, "nsInfo entries require a string 'ns' field".to_string())),
            },
            _ => Err(CommandError::new(2, "nsInfo entries must be documents".to_string())),
        })
        .collect::<Result<_, _>>()?;
    let ordered = command_bool(command, "ordered", true);
    let errors_only = command_bool(command, "errorsOnly", false);

    let mut results = Vec::new();
    let mut n_errors = 0_i64;
    let mut n_inserted = 0_i64;
    let mut n_matched = 0_i64;
    let mut n_modified = 0_i64;
    let mut n_upserted = 0_i64;
    let mut n_deleted = 0_i64;

    // 整条 `bulkWrite` 收进一个写入批处理作用域：每条语句的写入立即对后序语句可见，
    // 但整条命令只在结束时落盘一次。语句失败仍保留已生效的写入，与逐条提交一致。
    store.begin_write_batch();
    let outcome = (|| -> Result<(), CommandError> {
        for (index, operation) in operations.iter().enumerate() {
            let BsonValue::Document(operation) = operation
            else {
                return Err(CommandError::new(2, "BulkWrite ops entry must be a document".to_string()));
            };
            let kind = ["insert", "update", "delete"]
                .into_iter()
                .find(|field| operation.get(field).is_some())
                .ok_or_else(|| CommandError::new(2, "BulkWrite ops entry must be an insert, update, or delete".to_string()))?;
            let Some(BsonValue::Int32(namespace_index)) = operation.get(kind)
            else {
                return Err(CommandError::new(
                    2,
                    format!("BulkWrite ops entry {} has an invalid nsInfo index.", render_shell_value(&BsonValue::Document(operation.clone()))),
                ));
            };
            let namespace_index = usize::try_from(*namespace_index).ok();
            let Some(namespace_index) = namespace_index.filter(|index| *index < names.len())
            else {
                return Err(CommandError::new(
                    2,
                    format!("BulkWrite ops entry {} has an invalid nsInfo index.", render_shell_value(&BsonValue::Document(operation.clone()))),
                ));
            };
            let namespace = &names[namespace_index];
            let (database, collection) = namespace
                .split_once('.')
                .ok_or_else(|| CommandError::new(2, format!("nsInfo namespace '{namespace}' is not fully qualified")))?;

            let reply = match kind {
                "insert" => {
                    let Some(document) = operation.get("document")
                    else {
                        return Err(CommandError::new(2, "BulkWrite insert entry requires a 'document' field".to_string()));
                    };
                    let mut request = BsonDocument::new();
                    request.push("insert".to_string(), BsonValue::String(collection.to_string()));
                    request.push("documents".to_string(), BsonValue::Array(vec![document.clone()]));
                    request.push("$db".to_string(), BsonValue::String(database.to_string()));
                    insert(&request, store)?
                }
                "update" => {
                    let Some(filter) = operation.get("filter")
                    else {
                        return Err(CommandError::new(2, "BulkWrite update entry requires a 'filter' field".to_string()));
                    };
                    let Some(update_mods) = operation.get("updateMods")
                    else {
                        return Err(CommandError::new(2, "BulkWrite update entry requires an 'updateMods' field".to_string()));
                    };
                    let mut specification = BsonDocument::new();
                    specification.push("q".to_string(), filter.clone());
                    specification.push("u".to_string(), update_mods.clone());
                    if let Some(value) = operation.get("multi") {
                        specification.push("multi".to_string(), value.clone());
                    }
                    if let Some(value) = operation.get("upsert") {
                        specification.push("upsert".to_string(), value.clone());
                    }
                    let mut request = BsonDocument::new();
                    request.push("update".to_string(), BsonValue::String(collection.to_string()));
                    request.push("updates".to_string(), BsonValue::Array(vec![BsonValue::Document(specification)]));
                    request.push("$db".to_string(), BsonValue::String(database.to_string()));
                    update(&request, store)?
                }
                _ => {
                    let Some(filter) = operation.get("filter")
                    else {
                        return Err(CommandError::new(2, "BulkWrite delete entry requires a 'filter' field".to_string()));
                    };
                    let limit = if command_bool(operation, "multi", false) { 0 } else { 1 };
                    let mut specification = BsonDocument::new();
                    specification.push("q".to_string(), filter.clone());
                    specification.push("limit".to_string(), BsonValue::Int32(limit));
                    let mut request = BsonDocument::new();
                    request.push("delete".to_string(), BsonValue::String(collection.to_string()));
                    request.push("deletes".to_string(), BsonValue::Array(vec![BsonValue::Document(specification)]));
                    request.push("$db".to_string(), BsonValue::String(database.to_string()));
                    delete(&request, store)?
                }
            };

            let write_errors = match reply.get("writeErrors") {
                Some(BsonValue::Array(errors)) => errors,
                _ => &Vec::new(),
            };
            let failed = !write_errors.is_empty();
            let mut result = BsonDocument::new();
            result.push("ok".to_string(), BsonValue::Double(if failed { 0.0 } else { 1.0 }));
            result.push("idx".to_string(), BsonValue::Int32(index as i32));
            if let Some(first) = write_errors.first() {
                let BsonValue::Document(first) = first
                else {
                    return Err(CommandError::new(2, "writeErrors entries must be documents".to_string()));
                };
                for (field, value) in &first.fields {
                    if field == "index" {
                        continue;
                    }
                    result.push(field.clone(), value.clone());
                }
                result.push("n".to_string(), BsonValue::Int32(0));
                n_errors += 1;
            }
            else {
                let n = reply_count(&reply, "n");
                match kind {
                    "insert" => {
                        n_inserted += n;
                        result.push("n".to_string(), count_value(n));
                    }
                    "update" => {
                        if let Some(upserted) = reply.get("upserted") {
                            n_upserted += 1;
                            result.push("n".to_string(), count_value(n));
                            let modified = reply_count(&reply, "nModified");
                            n_modified += modified;
                            result.push("nModified".to_string(), count_value(modified));
                            let upserted = match upserted {
                                BsonValue::Array(items) => items.first().and_then(|item| match item {
                                    BsonValue::Document(entry) => entry.get("_id").cloned(),
                                    _ => None,
                                }),
                                other => Some(other.clone()),
                            };
                            if let Some(identifier) = upserted {
                                let mut entry = BsonDocument::new();
                                entry.push("_id".to_string(), identifier);
                                result.push("upserted".to_string(), BsonValue::Document(entry));
                            }
                        }
                        else {
                            n_matched += n;
                            let modified = reply_count(&reply, "nModified");
                            n_modified += modified;
                            result.push("n".to_string(), count_value(n));
                            result.push("nModified".to_string(), count_value(modified));
                        }
                    }
                    _ => {
                        n_deleted += n;
                        result.push("n".to_string(), count_value(n));
                    }
                }
            }

            if !errors_only || failed {
                results.push(BsonValue::Document(result));
            }
            if failed && ordered {
                break;
            }
        }
        Ok(())
    })();
    // 无论语句执行成功与否都要结束作用域：已经生效的写入必须落盘，
    // 落盘失败则内存回到命令开始前的状态并向客户端报错。
    let durable = store.end_write_batch();
    outcome?;
    durable.map_err(|error| CommandError::new(2, format!("bulkWrite failed to persist: {error}")))?;

    let mut cursor = BsonDocument::new();
    cursor.push("id".to_string(), BsonValue::Int64(0));
    cursor.push("firstBatch".to_string(), BsonValue::Array(results));
    cursor.push("ns".to_string(), BsonValue::String("admin.$cmd.bulkWrite".to_string()));
    let mut response = BsonDocument::new();
    response.push("cursor".to_string(), BsonValue::Document(cursor));
    response.push("nErrors".to_string(), count_value(n_errors));
    response.push("nInserted".to_string(), count_value(n_inserted));
    response.push("nMatched".to_string(), count_value(n_matched));
    response.push("nModified".to_string(), count_value(n_modified));
    response.push("nUpserted".to_string(), count_value(n_upserted));
    response.push("nDeleted".to_string(), count_value(n_deleted));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}
