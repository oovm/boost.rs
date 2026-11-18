//! 索引命令与 TTL：`createIndexes` / `listIndexes` / `dropIndexes`、索引选项校验与 TTL 监控。

use super::*;

/// Stages one `c` entry per index a `createIndexes` really built.
pub(crate) fn stage_create_indexes_oplog(store: &dyn MongoStorage, namespace: &str, collection: &str, created: &[IndexSpec]) {
    if created.is_empty() || !store.oplog_enabled() {
        return;
    }
    let uuid = collection_uuid(namespace);
    for specification in created {
        let mut object = BsonDocument::new();
        object.push("createIndexes".to_string(), BsonValue::String(collection.to_string()));
        object.push("v".to_string(), BsonValue::Int32(2));
        object.push("key".to_string(), BsonValue::Document(specification.key.clone()));
        object.push("name".to_string(), BsonValue::String(specification.name.clone()));
        if specification.unique {
            object.push("unique".to_string(), BsonValue::Bool(true));
        }
        if specification.sparse == Some(true) {
            object.push("sparse".to_string(), BsonValue::Bool(true));
        }
        store.stage_oplog(OplogEntry::command(&command_namespace(namespace), Some(uuid.clone()), object, None));
    }
}

/// Bytes one index entry occupies in the catalog and in the key material it
/// owns: the `listIndexes` definition plus the length-prefixed key tuple and the
/// 8-byte record id every entry stores next to its key.
pub(crate) const INDEX_RECORD_ID_BYTES: i64 = 8;

/// Key bytes one document contributes to one index.
pub(crate) fn index_record_bytes(values: &[BsonValue]) -> i64 {
    index_tuple(values).len() as i64 + INDEX_RECORD_ID_BYTES
}

/// Per-index footprint in `listIndexes` order, `_id_` first.
///
/// The definition bytes come from the same document `listIndexes` reports, and
/// the entry bytes come from the key material every stored document really
/// contributes (`index_entries` expands multikey values exactly like the write
/// path does). The oplog is a capped collection without an `_id` index, so it
/// reports no indexes.
///
/// The values are logical key bytes, not `mongod`'s WiredTiger page allocation;
/// `KNOWN-DIFFERENCES.md` records the measured difference.
pub fn collection_index_sizes(store: &dyn MongoStorage, namespace: &str) -> Result<Vec<(String, i64)>, CommandError> {
    if store.oplog_namespace().as_deref() == Some(namespace) {
        return Ok(Vec::new());
    }
    let specifications = store.index_specs(namespace)?;
    let mut sizes: Vec<(String, i64)> = Vec::with_capacity(1 + specifications.len());
    sizes.push(("_id_".to_string(), encode_bson(&id_index_listing()).len() as i64));
    for specification in &specifications {
        sizes.push((specification.name.clone(), encode_bson(&stored_index_listing(specification)).len() as i64));
    }
    for (_, payload) in store.scan(namespace)? {
        let Ok(document) = parse_bson(&payload)
        else {
            continue;
        };
        if let Some(identifier) = document.get("_id") {
            sizes[0].1 += index_record_bytes(std::slice::from_ref(identifier));
        }
        for (position, specification) in specifications.iter().enumerate() {
            for values in index_entries(&document, specification) {
                sizes[position + 1].1 += index_record_bytes(&values);
            }
        }
    }
    Ok(sizes)
}

/// Index options read from one `createIndexes` specification, already validated.
pub(crate) fn index_type_error(specification: &BsonDocument, detail: String) -> CommandError {
    CommandError::new(
        14,
        format!("Error in specification {} :: caused by :: {detail}", render_shell_value(&BsonValue::Document(specification.clone()))),
    )
}

pub(crate) fn requested_bool(specification: &BsonDocument, field: &str, quirky: bool) -> Result<Option<bool>, CommandError> {
    match specification.get(field) {
        None => Ok(None),
        Some(BsonValue::Bool(value)) => Ok(Some(*value)),
        Some(other) => {
            let detail = if quirky {
                format!("The field '{field} has value {field}: {}, which is not convertible to bool", render_shell_value(other))
            }
            else {
                format!("The field 'name' must be a bool, but got {}", type_label(other))
            };
            Err(index_type_error(specification, detail))
        }
    }
}

pub(crate) fn requested_wildcard_projection(specification: &BsonDocument) -> Result<Option<BsonDocument>, CommandError> {
    match specification.get("wildcardProjection") {
        None => Ok(None),
        Some(BsonValue::Document(projection)) => Ok(Some(projection.clone())),
        Some(other) => Err(CommandError::new(
            14,
            format!(
                "BSON field 'createIndexes.indexes.wildcardProjection' is the wrong type '{}', expected type 'object'",
                bson_type_name(other)
            ),
        )),
    }
}

pub(crate) fn index_specification_display(specification: &BsonDocument) -> BsonValue {
    // `mongod` echoes the specification in the order the caller sent it.
    let mut display = specification.clone();
    // `v: 2` appears only in the wildcard-specific validation messages.
    let wildcard = match specification.get("key") {
        Some(BsonValue::Document(key)) => key.fields.iter().any(|(field, _)| field == "$**" || field.ends_with(".$**")),
        _ => false,
    };
    if wildcard {
        display.push("v".to_string(), BsonValue::Int32(2));
    }
    BsonValue::Document(display)
}

/// `mongod` reports hashed-index validation failures from the index builder, so
/// the message carries the build wrapper and the normalized specification.
pub(crate) fn index_build_failure(code: i32, namespace: &str, specification: &BsonDocument, reason: &str) -> CommandError {
    let mut normalized = BsonDocument::new();
    normalized.push("v".to_string(), BsonValue::Int32(2));
    if matches!(specification.get("unique"), Some(BsonValue::Bool(true))) {
        normalized.push("unique".to_string(), BsonValue::Bool(true));
    }
    if let Some(key) = specification.get("key") {
        normalized.push("key".to_string(), key.clone());
    }
    if let Some(name) = specification.get("name") {
        normalized.push("name".to_string(), name.clone());
    }
    let uuid = collection_uuid_text(namespace);
    CommandError::new(
        code,
        format!(
            "Index build failed: {uuid}: Collection {namespace} ( {uuid} ) :: caused by :: Caught exception during index builder ((None)) initialization on namespace{namespace} ({uuid}). 1 index specs provided. First index spec: {} :: caused by :: {reason}",
            render_shell_value(&BsonValue::Document(normalized))
        ),
    )
}

/// Validate the index key pattern the way `mongod` does: numeric directions
/// must be non-zero, string values are index plugins, and wildcard keys are
/// `$**` or `path.$**`. Text indexes are not implemented yet and are refused
/// explicitly rather than silently stored under the wrong catalog shape.
pub(crate) fn validate_index_key(
    key: &BsonDocument,
    specification: &BsonDocument,
    namespace: &str,
    unique: bool,
    sparse: Option<bool>,
) -> Result<(), CommandError> {
    let mut hashed = 0usize;
    let mut text = false;
    let mut wildcard = false;
    for (field, value) in &key.fields {
        if field == "$**" || field.ends_with(".$**") {
            wildcard = true;
        }
        match value {
            BsonValue::Int32(number) if *number == 0 => {
                return Err(CommandError::new(
                    67,
                    format!(
                        "Error in specification {} :: caused by :: Values in the index key pattern cannot be 0.",
                        render_shell_value(&index_specification_display(specification))
                    ),
                ));
            }
            BsonValue::Int64(number) if *number == 0 => {
                return Err(CommandError::new(
                    67,
                    format!(
                        "Error in specification {} :: caused by :: Values in the index key pattern cannot be 0.",
                        render_shell_value(&index_specification_display(specification))
                    ),
                ));
            }
            BsonValue::Double(number) if *number == 0.0 => {
                return Err(CommandError::new(
                    67,
                    format!(
                        "Error in specification {} :: caused by :: Values in the index key pattern cannot be 0.",
                        render_shell_value(&index_specification_display(specification))
                    ),
                ));
            }
            BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) => {}
            BsonValue::String(plugin) => match plugin.as_str() {
                "hashed" => hashed += 1,
                "text" => text = true,
                "2dsphere" | "2d" | "geoHaystack" => {}
                other => {
                    return Err(CommandError::new(
                        67,
                        format!(
                            "Error in specification {} :: caused by :: Unknown index plugin '{other}'",
                            render_shell_value(&index_specification_display(specification))
                        ),
                    ));
                }
            },
            other => {
                return Err(CommandError::new(
                    67,
                    format!(
                        "Error in specification {} :: caused by :: Values in the index key pattern must be 1 or -1 or a supported index plugin name, but found {}",
                        render_shell_value(&index_specification_display(specification)),
                        bson_type_name(other)
                    ),
                ));
            }
        }
    }
    if text {
        return Err(CommandError::new(59, "text indexes are not supported yet"));
    }
    if hashed > 1 {
        let mut key_text = String::new();
        for (field, value) in &key.fields {
            if !key_text.is_empty() {
                key_text.push_str(", ");
            }
            key_text.push_str(field);
            key_text.push_str(": ");
            key_text.push_str(&render_shell_value(value));
        }
        return Err(index_build_failure(
            31303,
            namespace,
            specification,
            &format!("A maximum of one index field is allowed to be hashed but found {hashed} for 'key' {{ {key_text} }}"),
        ));
    }
    if wildcard {
        if unique {
            return Err(CommandError::new(
                67,
                format!(
                    "Error in specification {} :: caused by :: Index type 'wildcard' does not support the 'unique' option",
                    render_shell_value(&index_specification_display(specification))
                ),
            ));
        }
        if sparse == Some(true) {
            return Err(CommandError::new(
                67,
                format!(
                    "Error in specification {} :: caused by :: Index type 'wildcard' does not support the 'sparse' option",
                    render_shell_value(&index_specification_display(specification))
                ),
            ));
        }
    }
    if hashed == 1 && unique {
        return Err(index_build_failure(
            16764,
            namespace,
            specification,
            "Currently hashed indexes cannot guarantee uniqueness. Use a regular index.",
        ));
    }
    Ok(())
}

pub(crate) fn requested_ttl(specification: &BsonDocument) -> Result<Option<i32>, CommandError> {
    let Some(value) = specification.get("expireAfterSeconds")
    else {
        return Ok(None);
    };
    let numeric = match value {
        BsonValue::Int32(seconds) => i64::from(*seconds),
        BsonValue::Int64(seconds) => *seconds,
        BsonValue::Double(seconds) => *seconds as i64,
        other => {
            return Err(CommandError::new(
                67,
                format!(
                    ". Index spec: {} :: caused by :: TTL index 'expireAfterSeconds' option must be numeric, but received a type of '{}",
                    render_shell_value(&BsonValue::Document(specification.clone())),
                    type_label(other)
                ),
            ));
        }
    };
    if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&numeric) {
        return Err(CommandError::new(
            67,
            format!(
                ". Index spec: {} :: caused by :: TTL index 'expireAfterSeconds' must be within the range of a 32-bit integer",
                render_shell_value(&BsonValue::Document(specification.clone()))
            ),
        ));
    }
    if numeric < 0 {
        return Err(CommandError::new(
            67,
            format!(
                ". Index spec: {} :: caused by :: TTL index 'expireAfterSeconds' option cannot be less than 0",
                render_shell_value(&BsonValue::Document(specification.clone()))
            ),
        ));
    }
    Ok(Some(numeric as i32))
}

pub(crate) fn type_label(value: &BsonValue) -> &'static str {
    match value {
        BsonValue::Document(_) => "object",
        BsonValue::Array(_) => "array",
        BsonValue::String(_) => "string",
        BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) => "number",
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

/// Normalizes one `collation` document the way `listIndexes` reports it.
pub(crate) fn requested_collation(specification: &BsonDocument) -> Result<Option<BsonDocument>, CommandError> {
    let Some(value) = specification.get("collation")
    else {
        return Ok(None);
    };
    let BsonValue::Document(collation) = value
    else {
        return Err(index_type_error(specification, format!("The field 'collation' must be an object, but got {}", type_label(value))));
    };
    let mut definition = specification.clone();
    definition.push("v".to_string(), BsonValue::Int32(2));
    let failed = |code: i32, detail: String| {
        CommandError::new(
            code,
            format!(
                "failed to add collation information to index spec for index creation: {} :: caused by :: {detail}",
                render_shell_value(&BsonValue::Document(definition.clone()))
            ),
        )
    };
    let Some(BsonValue::String(locale)) = collation.get("locale")
    else {
        if collation.get("locale").is_none() {
            return Err(failed(40414, "BSON field 'collation.locale' is missing but a required field".to_string()));
        }
        return Err(failed(2, format!("Field 'locale' is invalid in: {}", render_shell_value(&BsonValue::Document(collation.clone())))));
    };
    if !known_locale(locale) {
        return Err(failed(2, format!("Field 'locale' is invalid in: {}", render_shell_value(&BsonValue::Document(collation.clone())))));
    }
    let strength = match collation.get("strength") {
        None => 3,
        Some(BsonValue::Int32(value)) => i64::from(*value),
        Some(BsonValue::Int64(value)) => *value,
        Some(BsonValue::Double(value)) => *value as i64,
        Some(other) => {
            return Err(failed(2, format!("BSON field 'strength' value must be a number, actual value '{}'", render_shell_value(other))));
        }
    };
    if !(1..=5).contains(&strength) {
        let detail = if strength > 5 {
            format!("BSON field 'strength' value must be <= 5, actual value '{strength}'")
        }
        else {
            format!("BSON field 'strength' value must be >= 1, actual value '{strength}'")
        };
        return Err(failed(2, detail));
    }
    let flag = |field: &str| matches!(collation.get(field), Some(BsonValue::Bool(true)));
    let case_first = match collation.get("caseFirst") {
        Some(BsonValue::String(value)) if ["off", "upper", "lower"].contains(&value.as_str()) => value.clone(),
        _ => "off".to_string(),
    };
    let alternate = match collation.get("alternate") {
        Some(BsonValue::String(value)) if ["non-ignorable", "shifted"].contains(&value.as_str()) => value.clone(),
        _ => "non-ignorable".to_string(),
    };
    let max_variable = match collation.get("maxVariable") {
        Some(BsonValue::String(value)) if ["punct", "space"].contains(&value.as_str()) => value.clone(),
        _ => "punct".to_string(),
    };
    let mut normalized = BsonDocument::new();
    normalized.push("locale".to_string(), BsonValue::String(locale.clone()));
    normalized.push("caseLevel".to_string(), BsonValue::Bool(flag("caseLevel")));
    normalized.push("caseFirst".to_string(), BsonValue::String(case_first));
    normalized.push("strength".to_string(), BsonValue::Int32(strength as i32));
    normalized.push("numericOrdering".to_string(), BsonValue::Bool(flag("numericOrdering")));
    normalized.push("alternate".to_string(), BsonValue::String(alternate));
    normalized.push("maxVariable".to_string(), BsonValue::String(max_variable));
    normalized.push("normalization".to_string(), BsonValue::Bool(flag("normalization")));
    normalized.push("backwards".to_string(), BsonValue::Bool(flag("backwards")));
    normalized.push("version".to_string(), BsonValue::String("57.1".to_string()));
    Ok(Some(normalized))
}

/// ICU knows far more locales than this gateway needs; the corpus uses `en`, and
/// an obviously synthetic token such as `bogus` must not silently pass.
pub(crate) fn known_locale(locale: &str) -> bool {
    matches!(
        locale,
        "en" | "en_US"
            | "fr"
            | "fr_FR"
            | "es"
            | "de"
            | "it"
            | "pt"
            | "nl"
            | "sv"
            | "da"
            | "fi"
            | "no"
            | "zh"
            | "zh_Hans"
            | "zh_Hant"
            | "ja"
            | "ko"
            | "ru"
            | "ar"
            | "hi"
            | "th"
            | "vi"
            | "tr"
            | "pl"
            | "cs"
            | "hu"
            | "ro"
            | "el"
            | "he"
            | "id"
            | "ms"
            | "uk"
            | "simple"
    )
}

pub(crate) fn partial_filter_from(specification: &BsonDocument) -> Result<Option<BsonDocument>, CommandError> {
    let Some(value) = specification.get("partialFilterExpression")
    else {
        return Ok(None);
    };
    let BsonValue::Document(filter) = value
    else {
        return Err(index_type_error(
            specification,
            format!("The field 'partialFilterExpression' must be an object, but got {}", type_label(value)),
        ));
    };
    if let Err(error) = validate_filter(filter, "createIndexes") {
        // `createIndexes` reports the bare operator message; the field/operator
        // indices `validate_filter` appends belong to query commands.
        let message = error.message.split(" (operation:").next().unwrap_or(&error.message);
        return Err(CommandError::new(
            error.code,
            format!("Error in specification {} :: caused by :: {message}", render_shell_value(&BsonValue::Document(specification.clone()))),
        ));
    }
    Ok(Some(filter.clone()))
}

pub(crate) fn is_id_key(key: &BsonDocument) -> bool {
    key.fields.len() == 1
        && key.fields[0].0 == "_id"
        && matches!(key.fields[0].1, BsonValue::Int32(1) | BsonValue::Int64(1) | BsonValue::Double(1.0))
}

pub(crate) fn same_index_options(left: &IndexSpec, right: &IndexSpec) -> bool {
    // `hidden` is not part of index equivalence on `mongod 8.0.9`: a visible
    // index and a hidden index over the same key are the same index.
    left.key == right.key
        && left.unique == right.unique
        && left.is_sparse() == right.is_sparse()
        && left.expire_after_seconds == right.expire_after_seconds
        && left.collation == right.collation
        && left.partial_filter_expression == right.partial_filter_expression
        && left.wildcard_projection == right.wildcard_projection
}

pub(crate) fn render_index_definition(specification: &IndexSpec) -> String {
    let mut definition = BsonDocument::new();
    definition.push("v".to_string(), BsonValue::Int32(2));
    if specification.unique {
        definition.push("unique".to_string(), BsonValue::Bool(true));
    }
    if specification.hidden {
        definition.push("hidden".to_string(), BsonValue::Bool(true));
    }
    definition.push("key".to_string(), BsonValue::Document(specification.key.clone()));
    definition.push("name".to_string(), BsonValue::String(specification.name.clone()));
    if let Some(sparse) = specification.sparse {
        definition.push("sparse".to_string(), BsonValue::Bool(sparse));
    }
    if let Some(seconds) = specification.expire_after_seconds {
        definition.push("expireAfterSeconds".to_string(), BsonValue::Int32(seconds));
    }
    if let Some(collation) = &specification.collation {
        definition.push("collation".to_string(), BsonValue::Document(collation.clone()));
    }
    if let Some(filter) = &specification.partial_filter_expression {
        definition.push("partialFilterExpression".to_string(), BsonValue::Document(filter.clone()));
    }
    if let Some(projection) = &specification.wildcard_projection {
        definition.push("wildcardProjection".to_string(), BsonValue::Document(projection.clone()));
    }
    render_shell_value(&BsonValue::Document(definition))
}

/// `createIndexes` for single-field and compound keys, `unique`, and explicit
/// names.
///
/// `name` is required: `mongod 8.0.9` rejects a specification without it, so
/// this gateway does not invent a default name either.
pub fn create_indexes(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace = namespace_from_command(command, "createIndexes")
        .ok_or_else(|| CommandError::new(73, "createIndexes command missing collection name"))?;
    let Some(BsonValue::Array(specifications)) = command.get("indexes")
    else {
        return Err(CommandError::new(40414, "BSON field 'createIndexes.indexes' is missing but a required field"));
    };
    if specifications.is_empty() {
        return Err(CommandError::new(2, "Must specify at least one index to create"));
    }

    let existing = store.index_specs(&namespace)?;
    let existed = store.namespace_exists(&namespace)?;
    let mut known = existing.clone();
    let mut accepted: Vec<IndexSpec> = Vec::new();
    let mut skipped = 0_usize;

    for item in specifications {
        let BsonValue::Document(specification) = item
        else {
            return Err(CommandError::new(2, "index specification must be a document"));
        };
        let Some(BsonValue::Document(key)) = specification.get("key")
        else {
            return Err(CommandError::new(
                9,
                format!(
                    "Error in specification {} :: caused by :: The 'key' field is a required property of an index specification",
                    render_shell_value(&BsonValue::Document(specification.clone()))
                ),
            ));
        };
        if key.fields.is_empty() {
            return Err(CommandError::new(
                67,
                format!(
                    "Error in specification {} :: caused by :: Index keys cannot be empty.",
                    render_shell_value(&BsonValue::Document(specification.clone()))
                ),
            ));
        }
        let Some(BsonValue::String(name)) = specification.get("name")
        else {
            return Err(CommandError::new(
                9,
                format!(
                    "Error in specification {} :: caused by :: The 'name' field is a required property of an index specification",
                    render_shell_value(&BsonValue::Document(specification.clone()))
                ),
            ));
        };
        if name.is_empty() {
            let mut rendered = specification.clone();
            rendered.push("v".to_string(), BsonValue::Int32(2));
            return Err(CommandError::new(
                67,
                format!(
                    "Error in specification {} :: caused by :: index name cannot be empty",
                    render_shell_value(&BsonValue::Document(rendered))
                ),
            ));
        }
        let unique = requested_bool(specification, "unique", true)?.unwrap_or(false);
        let sparse = requested_bool(specification, "sparse", true)?;
        let hidden = requested_bool(specification, "hidden", false)?.unwrap_or(false);
        let expire_after_seconds = requested_ttl(specification)?;
        let collation = requested_collation(specification)?;
        let partial_filter_expression = partial_filter_from(specification)?;
        let wildcard_projection = requested_wildcard_projection(specification)?;
        validate_index_key(key, specification, &namespace, unique, sparse)?;

        let mut candidate = IndexSpec {
            namespace: namespace.clone(),
            name: name.clone(),
            key: key.clone(),
            unique,
            sparse,
            hidden,
            expire_after_seconds,
            collation,
            partial_filter_expression,
            wildcard_projection,
            multikey: false,
        };

        if is_id_key(key) {
            // `_id` is always indexed under the implicit `_id_` name. A second
            // specification for it never creates a second index.
            if candidate.expire_after_seconds.is_some() {
                let mut definition = specification.clone();
                definition.push("v".to_string(), BsonValue::Int32(2));
                return Err(CommandError::new(
                    197,
                    format!(
                        "The field 'expireAfterSeconds' is not valid for an _id index specification. Specification: {}",
                        render_shell_value(&BsonValue::Document(definition))
                    ),
                ));
            }
            skipped += 1;
            continue;
        }
        if candidate.expire_after_seconds.is_some() && candidate.key.fields.len() != 1 {
            return Err(CommandError::new(
                67,
                format!(
                    "TTL indexes are single-field indexes, compound indexes do not support TTL. Index spec: {}",
                    render_shell_value(&BsonValue::Document(specification.clone()))
                ),
            ));
        }

        if let Some(hit) = known.iter().find(|index| index.name == *name) {
            if same_index_options(hit, &candidate) {
                skipped += 1;
                continue;
            }
            return Err(CommandError::new(
                86,
                format!(
                    "An existing index has the same name as the requested index. When index names are not specified, they are auto generated and can cause conflicts. Please refer to our documentation. Requested index: {}, existing index: {}",
                    render_index_definition(&candidate),
                    render_index_definition(hit)
                ),
            ));
        }
        if let Some(hit) = known.iter().find(|index| same_index_options(index, &candidate)) {
            if hit.hidden == candidate.hidden {
                return Err(CommandError::new(85, format!("Index already exists with a different name: {}", hit.name)));
            }
            return Err(CommandError::new(
                85,
                format!(
                    "Index build failed: 00000000-0000-0000-0000-000000000000: Collection {namespace} ( 00000000-0000-0000-0000-000000000000 ) :: caused by :: An equivalent index already exists with a different name and options. Requested index: {}, existing index: {}",
                    render_index_definition(&candidate),
                    render_index_definition(hit)
                ),
            ));
        }

        candidate.name = name.clone();
        if candidate.unique {
            verify_unique_build(store, &namespace, &candidate)?;
        }
        known.push(candidate.clone());
        accepted.push(candidate);
    }

    if !accepted.is_empty() {
        store.add_indexes(&namespace, &accepted).map_err(|error| CommandError::new(2, format!("createIndexes failed: {error}")))?;
        // 逐个数：新建索引需要回填，旧目录里缺条目的索引也在这里补齐，避免
        // 查询把「条目缺失」当成「没有匹配」。
        for specification in store.index_specs(&namespace)? {
            if !store.reserved_has_entries(&crate::mongo::storage::index_namespace(&namespace, &specification.name))? {
                backfill_index(store, &namespace, &specification)?;
            }
        }
        if let Some(BsonValue::String(collection)) = command.get("createIndexes") {
            stage_create_indexes_oplog(store, &namespace, collection, &accepted);
        }
    }

    let before = 1 + existing.len() as i64;
    let after = before + accepted.len() as i64;
    let mut reply = BsonDocument::new();
    reply.push("numIndexesBefore".to_string(), count_value(before));
    reply.push("numIndexesAfter".to_string(), count_value(after));
    if accepted.is_empty() {
        reply.push("note".to_string(), BsonValue::String("all indexes already exist".to_string()));
    }
    else {
        reply.push("createdCollectionAutomatically".to_string(), BsonValue::Bool(!existed));
        if skipped > 0 {
            reply.push("note".to_string(), BsonValue::String("index already exists".to_string()));
        }
    }
    reply.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(reply)
}

/// 用集合现有文档重建一个索引的全部条目。
///
/// 先清空同名索引项，避免上一次删除中断或崩溃留下的旧条目与新定义混在一起。
pub(crate) fn backfill_index(store: &dyn MongoStorage, namespace: &str, specification: &IndexSpec) -> Result<(), CommandError> {
    let entry_namespace = crate::mongo::storage::index_namespace(namespace, &specification.name);
    store.clear_reserved(&entry_namespace)?;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut multikey = false;
    for (document_key, payload) in store.scan(namespace)? {
        let Ok(document) = parse_bson(&payload)
        else {
            continue;
        };
        if !multikey {
            multikey = specification.key.fields.iter().any(|(field, _)| field_array_valued(&document, field));
        }
        for values in index_entries(&document, specification) {
            entries.push((index_entry_key(&values, &document_key), document_key.clone()));
        }
    }
    store.put_reserved(&entry_namespace, &entries).map_err(|error| CommandError::new(2, format!("createIndexes backfill failed: {error}")))?;
    if multikey && !specification.multikey {
        store
            .mark_index_multikey(namespace, &specification.name)
            .map_err(|error| CommandError::new(2, format!("createIndexes multikey marking failed: {error}")))?;
    }
    Ok(())
}

/// Refuses to build a unique index over documents that already collide.
pub(crate) fn verify_unique_build(store: &dyn MongoStorage, namespace: &str, specification: &IndexSpec) -> Result<(), CommandError> {
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (_, payload) in store.scan(namespace)? {
        let Ok(document) = parse_bson(&payload)
        else {
            continue;
        };
        for values in index_entries(&document, specification) {
            if !seen.insert(index_tuple(&values)) {
                let key_value =
                    specification.key.fields.iter().zip(values.iter()).fold(BsonDocument::new(), |mut document, ((field, _), value)| {
                        document.push(field.clone(), value.clone());
                        document
                    });
                return Err(CommandError::new(
                    11000,
                    format!(
                        "Index build failed: {}: Collection {} ( {} ) :: caused by :: {}",
                        seeded_uuid(&format!("{namespace}|{}|build", specification.name)),
                        namespace,
                        seeded_uuid(namespace),
                        duplicate_message(namespace, specification, &values)
                    ),
                )
                .with_detail("keyPattern", BsonValue::Document(specification.key.clone()))
                .with_detail("keyValue", BsonValue::Document(key_value)));
            }
        }
    }
    Ok(())
}

/// Deterministic UUID-shaped identifier.
///
/// `mongod` prints its random collection UUID and index-build operation UUID in
/// this message. Their concrete values are opaque and change per build, so this
/// gateway derives stable ones from the namespace instead: the message template
/// is byte-identical while the two UUID slots stay reproducible for tests.
pub(crate) fn seeded_uuid(seed: &str) -> String {
    let high = fnv1a64(0xcbf2_9ce4_8422_2325, seed);
    let low = fnv1a64(0x8422_2325_cbf2_9ce4, seed);
    let high = (high & 0xffff_ffff_ffff_0fff) | 0x0000_0000_0000_4000;
    let low = (low & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", high >> 32, (high >> 16) & 0xffff, high & 0xffff, low >> 48, low & 0xffff_ffff_ffff)
}

pub(crate) fn fnv1a64(offset: u64, text: &str) -> u64 {
    let mut hash = offset;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn duplicate_message(namespace: &str, specification: &IndexSpec, values: &[BsonValue]) -> String {
    let rendered: Vec<String> =
        specification.key.fields.iter().zip(values).map(|((field, _), value)| format!("{field}: {}", render_shell_value(value))).collect();
    format!("E11000 duplicate key error collection: {namespace} index: {} dup key: {{ {} }}", specification.name, rendered.join(", "))
}

/// `listIndexes` document for the implicit `_id_` index, which `mongod` reports
/// first and never stores in the catalog.
pub(crate) fn id_index_listing() -> BsonDocument {
    let mut identifier = BsonDocument::new();
    identifier.push("_id".to_string(), BsonValue::Int32(1));
    let mut entry = BsonDocument::new();
    entry.push("v".to_string(), BsonValue::Int32(2));
    entry.push("key".to_string(), BsonValue::Document(identifier));
    entry.push("name".to_string(), BsonValue::String("_id_".to_string()));
    entry
}

/// `listIndexes` document for one stored index, options included.
pub(crate) fn stored_index_listing(specification: &IndexSpec) -> BsonDocument {
    let mut entry = BsonDocument::new();
    entry.push("v".to_string(), BsonValue::Int32(2));
    entry.push("key".to_string(), BsonValue::Document(specification.key.clone()));
    entry.push("name".to_string(), BsonValue::String(specification.name.clone()));
    if let Some(projection) = &specification.wildcard_projection {
        entry.push("wildcardProjection".to_string(), BsonValue::Document(projection.clone()));
    }
    if specification.unique {
        entry.push("unique".to_string(), BsonValue::Bool(true));
    }
    if specification.hidden {
        entry.push("hidden".to_string(), BsonValue::Bool(true));
    }
    if let Some(sparse) = specification.sparse {
        entry.push("sparse".to_string(), BsonValue::Bool(sparse));
    }
    if let Some(collation) = &specification.collation {
        entry.push("collation".to_string(), BsonValue::Document(collation.clone()));
    }
    if let Some(filter) = &specification.partial_filter_expression {
        entry.push("partialFilterExpression".to_string(), BsonValue::Document(filter.clone()));
    }
    if let Some(seconds) = specification.expire_after_seconds {
        entry.push("expireAfterSeconds".to_string(), BsonValue::Int32(seconds));
    }
    entry
}

/// `listIndexes`: `_id_` first, then user indexes in creation order.
pub fn list_indexes(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace =
        namespace_from_command(command, "listIndexes").ok_or_else(|| CommandError::new(73, "listIndexes command missing collection name"))?;
    if !store.namespace_exists(&namespace)? {
        return Err(CommandError::new(26, format!("ns does not exist: {namespace}")));
    }

    let mut first_batch: Vec<BsonValue> = Vec::new();
    first_batch.push(BsonValue::Document(id_index_listing()));
    for specification in store.index_specs(&namespace)? {
        first_batch.push(BsonValue::Document(stored_index_listing(&specification)));
    }

    let mut cursor = BsonDocument::new();
    cursor.push("id".to_string(), BsonValue::Int64(0));
    cursor.push("ns".to_string(), BsonValue::String(namespace));
    cursor.push("firstBatch".to_string(), BsonValue::Array(first_batch));
    let mut reply = BsonDocument::new();
    reply.push("cursor".to_string(), BsonValue::Document(cursor));
    reply.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(reply)
}

/// `dropIndexes` by name, `*`, or key pattern; `_id_` is never droppable.
pub fn drop_indexes(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace =
        namespace_from_command(command, "dropIndexes").ok_or_else(|| CommandError::new(73, "dropIndexes command missing collection name"))?;
    if !store.namespace_exists(&namespace)? {
        return Err(CommandError::new(26, format!("ns not found {namespace}")));
    }
    let existing = store.index_specs(&namespace)?;
    let name = match command.get("index") {
        Some(BsonValue::String(name)) => name.clone(),
        Some(BsonValue::Document(key)) => {
            let mut identifier = BsonDocument::new();
            identifier.push("_id".to_string(), BsonValue::Int32(1));
            if key_pattern_matches(key, &identifier) {
                return Err(CommandError::new(72, "cannot drop _id index"));
            }
            match existing.iter().find(|index| key_pattern_matches(key, &index.key)) {
                Some(index) => index.name.clone(),
                None => {
                    return Err(CommandError::new(
                        27,
                        format!("can't find index with key: {}", render_shell_value(&BsonValue::Document(key.clone()))),
                    ));
                }
            }
        }
        Some(other) => {
            return Err(CommandError::new(
                14,
                format!("BSON field 'dropIndexes.index' is the wrong type '{}', expected types '[string']", bson_type_name(other)),
            ));
        }
        None => return Err(CommandError::new(40414, "BSON field 'dropIndexes.index' is missing but a required field")),
    };

    let before = 1 + existing.len() as i64;
    if name == "_id_" {
        return Err(CommandError::new(72, "cannot drop _id index"));
    }
    if name != "*" && !existing.iter().any(|index| index.name == name) {
        return Err(CommandError::new(27, format!("index not found with name [{name}]")));
    }

    let dropped: Vec<IndexSpec> =
        if name == "*" { existing.clone() } else { existing.iter().filter(|index| index.name == name).cloned().collect() };
    store.drop_indexes(&namespace, &name).map_err(|error| CommandError::new(2, format!("dropIndexes failed: {error}")))?;
    for specification in &dropped {
        store
            .clear_reserved(&crate::mongo::storage::index_namespace(&namespace, &specification.name))
            .map_err(|error| CommandError::new(2, format!("dropIndexes cleanup failed: {error}")))?;
    }
    if store.oplog_enabled() {
        let uuid = collection_uuid(&namespace);
        for specification in &dropped {
            let mut object = BsonDocument::new();
            if let Some(BsonValue::String(collection)) = command.get("dropIndexes") {
                object.push("dropIndexes".to_string(), BsonValue::String(collection.clone()));
            }
            object.push("index".to_string(), BsonValue::String(name.clone()));
            let mut old = BsonDocument::new();
            old.push("v".to_string(), BsonValue::Int32(2));
            old.push("key".to_string(), BsonValue::Document(specification.key.clone()));
            old.push("name".to_string(), BsonValue::String(specification.name.clone()));
            store.stage_oplog(OplogEntry::command(&command_namespace(&namespace), Some(uuid.clone()), object, Some(old)));
        }
    }

    let mut reply = BsonDocument::new();
    reply.push("nIndexesWas".to_string(), count_value(before));
    if name == "*" {
        reply.push("msg".to_string(), BsonValue::String("non-_id indexes dropped for collection".to_string()));
    }
    reply.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(reply)
}

/// Outcome of one TTL monitor pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TtlReport {
    /// Whether the sleep window had elapsed, so a pass actually ran.
    pub scanned: bool,
    /// Documents removed by this pass.
    pub deleted: usize,
    /// Per-namespace removal counts, in catalog order.
    pub collections: Vec<(String, usize)>,
}

/// One pass of the TTL monitor.
///
/// `mongod 8.0.9` removes `expireAfterSeconds` documents from a background
/// thread that wakes every `ttlMonitorSleepSecs` (default 60). A document whose
/// deadline falls between two wake-ups is removed by the next wake-up, so the
/// visible delay is `[0, ttlMonitorSleepSecs]` rather than millisecond exact.
/// This primitive keeps that shape: it returns without scanning unless the
/// window since the previous pass has elapsed, so a document never disappears
/// merely because its own deadline passed.
///
/// Hosts call it from their own loop and pass `now_ms`; that caller-supplied
/// clock is also the only clock the scan reads, so tests can compress the
/// window without changing the semantics.
///
/// Only a single-field index expires documents. An indexed value that is not a
/// [`BsonValue::DateTime`] never expires, and array values expire when any one
/// element is a date past the deadline.
pub fn ttl_monitor(store: &dyn MongoStorage, now_ms: i64) -> Result<TtlReport, CommandError> {
    let mut report = TtlReport::default();
    if !store.claim_ttl_pass(now_ms) {
        return Ok(report);
    }
    report.scanned = true;

    for namespace in store.namespaces()? {
        let mut deleted = 0_usize;
        for specification in store.index_specs(&namespace)? {
            let Some(seconds) = specification.expire_after_seconds
            else {
                continue;
            };
            // `mongod` creates a compound TTL index but never expires documents
            // from it. A hidden index still expires: `hidden` only keeps the
            // index out of the planner.
            if specification.key.fields.len() != 1 {
                continue;
            }
            let Some((field, _)) = specification.key.fields.first()
            else {
                continue;
            };
            let threshold = now_ms.saturating_sub(i64::from(seconds) * 1_000);
            let mut candidates: Vec<Vec<u8>> = Vec::new();
            for (key, payload) in store.scan(&namespace)? {
                let Ok(document) = parse_bson(&payload)
                else {
                    continue;
                };
                if let Some(filter) = &specification.partial_filter_expression
                    && !document_matches(&document, filter).unwrap_or(false)
                {
                    continue;
                }
                let expired = field_values(&document, field).iter().any(|value| match value {
                    BsonValue::DateTime(milliseconds) => *milliseconds <= threshold,
                    _ => false,
                });
                if expired {
                    candidates.push(key);
                }
            }
            if candidates.is_empty() {
                continue;
            }
            deleted += stage_delete(store, None, &namespace, &candidates)?;
        }
        if deleted > 0 {
            report.deleted += deleted;
            report.collections.push((namespace, deleted));
        }
    }
    Ok(report)
}

/// Host-facing document for one monitor pass, shared by both bindings.
pub fn ttl_report_document(report: &TtlReport) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("scanned".to_string(), BsonValue::Bool(report.scanned));
    document.push("deleted".to_string(), BsonValue::Int32(report.deleted as i32));
    let collections = report
        .collections
        .iter()
        .map(|(namespace, count)| {
            let mut entry = BsonDocument::new();
            entry.push("ns".to_string(), BsonValue::String(namespace.clone()));
            entry.push("deleted".to_string(), BsonValue::Int32(*count as i32));
            BsonValue::Document(entry)
        })
        .collect();
    document.push("collections".to_string(), BsonValue::Array(collections));
    document
}
