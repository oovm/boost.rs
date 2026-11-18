//! 目录命令：`listCollections` / `listDatabases` / `create` / `drop` / `collMod`、
//! schema 校验与 `dbStats` / `collStats`。

use super::*;

/// `listCollections` with `nameOnly`, `authorizedCollections`, and a general
/// `filter` applied to the rendered collection descriptions.
pub fn list_collections(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let database = command.database();
    let prefix = format!("{database}.");
    let name_only = match command.get("nameOnly") {
        Some(BsonValue::Bool(flag)) => *flag,
        Some(other) => {
            return Err(CommandError::new(
                14,
                format!("BSON field 'listCollections.nameOnly' is the wrong type '{}', expected type 'bool'", bson_type_name(other)),
            ));
        }
        None => false,
    };
    if let Some(other) = command.get("authorizedCollections")
        && !matches!(other, BsonValue::Bool(_))
    {
        return Err(CommandError::new(
            14,
            format!("BSON field 'listCollections.authorizedCollections' is the wrong type '{}', expected type 'bool'", bson_type_name(other)),
        ));
    }
    let filter = match command.get("filter") {
        Some(BsonValue::Document(filter)) => {
            validate_filter(filter, "listCollections")?;
            Some(filter)
        }
        _ => None,
    };

    let mut entries = Vec::new();
    for namespace in store.namespaces()? {
        let Some(collection) = namespace.strip_prefix(&prefix)
        else {
            continue;
        };
        let mut entry = BsonDocument::new();
        entry.push("name".to_string(), BsonValue::String(collection.to_string()));
        entry.push("type".to_string(), BsonValue::String("collection".to_string()));
        if name_only {
            if let Some(filter) = filter
                && !document_matches(&entry, filter)?
            {
                continue;
            }
            entries.push(BsonValue::Document(entry));
            continue;
        }
        let is_oplog = store.oplog_namespace().as_deref() == Some(namespace.as_str());
        let mut options = BsonDocument::new();
        if is_oplog {
            if let Some((_, size, _, _)) = store.oplog_status() {
                options.push("capped".to_string(), BsonValue::Bool(true));
                options.push("size".to_string(), BsonValue::Int64(size));
                options.push("autoIndexId".to_string(), BsonValue::Bool(false));
            }
        }
        else {
            if let Some(validator) = collection_validator_value(store, &namespace)? {
                options.push("validator".to_string(), validator);
            }
            let stored = store.collection_options(&namespace)?;
            if let Some(level) = &stored.validation_level {
                options.push("validationLevel".to_string(), BsonValue::String(level.clone()));
            }
            if let Some(action) = &stored.validation_action {
                options.push("validationAction".to_string(), BsonValue::String(action.clone()));
            }
        }
        entry.push("options".to_string(), BsonValue::Document(options));
        let mut info = BsonDocument::new();
        info.push("readOnly".to_string(), BsonValue::Bool(false));
        info.push("uuid".to_string(), collection_uuid(&namespace));
        entry.push("info".to_string(), BsonValue::Document(info));
        if !is_oplog {
            let mut id_index = BsonDocument::new();
            id_index.push("v".to_string(), BsonValue::Int32(2));
            let mut key = BsonDocument::new();
            key.push("_id".to_string(), BsonValue::Int32(1));
            id_index.push("key".to_string(), BsonValue::Document(key));
            id_index.push("name".to_string(), BsonValue::String("_id_".to_string()));
            entry.push("idIndex".to_string(), BsonValue::Document(id_index));
        }
        if let Some(filter) = filter
            && !document_matches(&entry, filter)?
        {
            continue;
        }
        entries.push(BsonValue::Document(entry));
    }

    Ok(cursor_reply(&format!("{database}.$cmd.listCollections"), "firstBatch", entries, 0))
}

/// A stable collection identifier derived from the namespace, so reopening a
/// store keeps the `info.uuid` the catalog reported before.
pub(crate) fn collection_uuid_bytes(namespace: &str) -> [u8; 16] {
    let mut high: u64 = 0xcbf2_9ce4_8422_2325;
    let mut low: u64 = 0x9e37_79b9_7f4a_7c15;
    for byte in namespace.bytes() {
        high ^= u64::from(byte);
        high = high.wrapping_mul(0x0000_0100_0000_01b3);
        low ^= u64::from(byte).wrapping_add(0x9e37_79b9);
        low = low.rotate_left(7).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes
}

pub(crate) fn collection_uuid(namespace: &str) -> BsonValue {
    BsonValue::Binary(BinaryData::new(4, collection_uuid_bytes(namespace).to_vec()))
}

/// UUID in the `8-4-4-4-12` textual form `mongod` uses inside error messages.
pub(crate) fn collection_uuid_text(namespace: &str) -> String {
    let bytes = collection_uuid_bytes(namespace);
    format!(
        "{}-{}-{}-{}-{}",
        bytes[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        bytes[4..6].iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        bytes[6..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        bytes[8..10].iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        bytes[10..].iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    )
}

/// `mongod 8.0.9` refuses a second `create` whose options differ from the
/// stored ones and quotes the stored options in the message, uuid first.
pub(crate) fn create_conflict_message(namespace: &str, options: &CollectionOptions) -> String {
    let mut rendered = vec![format!("uuid: UUID(\"{}\")", collection_uuid_text(namespace))];
    if let Some(validator) = &options.validator
        && let Ok(value) = json::from_json(validator)
    {
        rendered.push(format!("validator: {}", render_shell_value(&value)));
    }
    if let Some(level) = &options.validation_level {
        rendered.push(format!("validationLevel: \"{level}\""));
    }
    if let Some(action) = &options.validation_action {
        rendered.push(format!("validationAction: \"{action}\""));
    }
    format!("namespace {namespace} already exists, but with different options: {{ {} }}", rendered.join(", "))
}

pub(crate) fn collection_validator_value(store: &dyn MongoStorage, namespace: &str) -> Result<Option<BsonValue>, CommandError> {
    Ok(store.collection_options(namespace)?.validator.and_then(|validator| json::from_json(&validator).ok()))
}

/// The validator schema, level, and action in force for a namespace. A missing
/// `$jsonSchema` validator means no schema validation happens.
pub(crate) fn collection_schema(store: &dyn MongoStorage, namespace: &str) -> Result<Option<(BsonDocument, String, String)>, CommandError> {
    let options = store.collection_options(namespace)?;
    let Some(validator) = options.validator.and_then(|value| json::from_json(&value).ok())
    else {
        return Ok(None);
    };
    let BsonValue::Document(validator) = validator
    else {
        return Ok(None);
    };
    let schema = match validator.get("$jsonSchema") {
        Some(BsonValue::Document(schema)) => schema.clone(),
        _ => return Ok(None),
    };
    let level = options.validation_level.unwrap_or_else(|| "strict".to_string());
    let action = options.validation_action.unwrap_or_else(|| "error".to_string());
    Ok(Some((schema, level, action)))
}

pub(crate) fn schema_rules(schema: &BsonDocument, document: &BsonDocument) -> Vec<BsonValue> {
    let mut rules = Vec::new();
    if let Some(BsonValue::String(expected)) = schema.get("bsonType")
        && expected != "object"
    {
        let mut specified = BsonDocument::new();
        specified.push("$jsonSchema".to_string(), BsonValue::Document(schema.clone()));
        let mut rule = BsonDocument::new();
        rule.push("operatorName".to_string(), BsonValue::String("$jsonSchema".to_string()));
        rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
        rule.push("reason".to_string(), BsonValue::String("expression did not evaluate to true".to_string()));
        rules.push(BsonValue::Document(rule));
        return rules;
    }
    if let Some(BsonValue::Bool(false)) = schema.get("additionalProperties") {
        let properties = match schema.get("properties") {
            Some(BsonValue::Document(properties)) => properties,
            _ => &BsonDocument::new(),
        };
        let extra: Vec<String> =
            document.fields.iter().filter(|(field, _)| properties.get(field).is_none()).map(|(field, _)| field.clone()).collect();
        if !extra.is_empty() {
            let mut specified = BsonDocument::new();
            specified.push("additionalProperties".to_string(), BsonValue::Bool(false));
            let mut rule = BsonDocument::new();
            rule.push("operatorName".to_string(), BsonValue::String("additionalProperties".to_string()));
            rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
            rule.push("additionalProperties".to_string(), BsonValue::Array(extra.into_iter().map(BsonValue::String).collect()));
            rules.push(BsonValue::Document(rule));
        }
    }
    if let Some(BsonValue::Array(required)) = schema.get("required") {
        let missing: Vec<BsonValue> = required
            .iter()
            .filter_map(|requirement| match requirement {
                BsonValue::String(field) if document.get(field).is_none() => Some(BsonValue::String(field.clone())),
                _ => None,
            })
            .collect();
        if !missing.is_empty() {
            let mut specified = BsonDocument::new();
            specified.push("required".to_string(), BsonValue::Array(required.clone()));
            let mut rule = BsonDocument::new();
            rule.push("operatorName".to_string(), BsonValue::String("required".to_string()));
            rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
            rule.push("missingProperties".to_string(), BsonValue::Array(missing));
            rules.push(BsonValue::Document(rule));
        }
    }
    if let Some(BsonValue::Document(properties)) = schema.get("properties") {
        let mut unsatisfied = Vec::new();
        for (field, property_schema) in &properties.fields {
            let BsonValue::Document(property_schema) = property_schema
            else {
                continue;
            };
            let Some(value) = document.get(field)
            else {
                continue;
            };
            let details = schema_details(property_schema, value);
            if details.is_empty() {
                continue;
            }
            let mut entry = BsonDocument::new();
            entry.push("propertyName".to_string(), BsonValue::String(field.clone()));
            entry.push("details".to_string(), BsonValue::Array(details.into_iter().map(BsonValue::Document).collect()));
            unsatisfied.push(BsonValue::Document(entry));
        }
        if !unsatisfied.is_empty() {
            let mut rule = BsonDocument::new();
            rule.push("operatorName".to_string(), BsonValue::String("properties".to_string()));
            rule.push("propertiesNotSatisfied".to_string(), BsonValue::Array(unsatisfied));
            rules.push(BsonValue::Document(rule));
        }
    }
    rules
}

pub(crate) fn schema_details(schema: &BsonDocument, value: &BsonValue) -> Vec<BsonDocument> {
    let mut details = Vec::new();
    if let Some(BsonValue::String(expected)) = schema.get("bsonType") {
        let actual = bson_type_name(value);
        if actual != expected {
            let mut specified = BsonDocument::new();
            specified.push("bsonType".to_string(), BsonValue::String(expected.clone()));
            let mut rule = BsonDocument::new();
            rule.push("operatorName".to_string(), BsonValue::String("bsonType".to_string()));
            rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
            rule.push("reason".to_string(), BsonValue::String("type did not match".to_string()));
            rule.push("consideredValue".to_string(), value.clone());
            rule.push("consideredType".to_string(), BsonValue::String(actual.to_string()));
            details.push(rule);
            return details;
        }
    }
    if let Some(minimum) = schema.get("minimum")
        && let (Some(current), Some(limit)) = (number_of(value), number_of(minimum))
        && current < limit
    {
        let mut specified = BsonDocument::new();
        specified.push("minimum".to_string(), minimum.clone());
        let mut rule = BsonDocument::new();
        rule.push("operatorName".to_string(), BsonValue::String("minimum".to_string()));
        rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
        rule.push("reason".to_string(), BsonValue::String("comparison failed".to_string()));
        rule.push("consideredValue".to_string(), value.clone());
        details.push(rule);
    }
    if let Some(maximum) = schema.get("maximum")
        && let (Some(current), Some(limit)) = (number_of(value), number_of(maximum))
        && current > limit
    {
        let mut specified = BsonDocument::new();
        specified.push("maximum".to_string(), maximum.clone());
        let mut rule = BsonDocument::new();
        rule.push("operatorName".to_string(), BsonValue::String("maximum".to_string()));
        rule.push("specifiedAs".to_string(), BsonValue::Document(specified));
        rule.push("reason".to_string(), BsonValue::String("comparison failed".to_string()));
        rule.push("consideredValue".to_string(), value.clone());
        details.push(rule);
    }
    if let (Some(BsonValue::Document(items)), BsonValue::Array(values)) = (schema.get("items"), value) {
        for (index, item) in values.iter().enumerate() {
            let item_details = schema_details(items, item);
            if item_details.is_empty() {
                continue;
            }
            let mut rule = BsonDocument::new();
            rule.push("operatorName".to_string(), BsonValue::String("items".to_string()));
            rule.push("reason".to_string(), BsonValue::String("At least one item did not match the sub-schema".to_string()));
            rule.push("itemIndex".to_string(), BsonValue::Int32(index as i32));
            rule.push("details".to_string(), BsonValue::Array(item_details.into_iter().map(BsonValue::Document).collect()));
            details.push(rule);
        }
    }
    details
}

pub(crate) fn validation_err_info(document: &BsonDocument, rules: Vec<BsonValue>) -> BsonDocument {
    let mut details = BsonDocument::new();
    details.push("operatorName".to_string(), BsonValue::String("$jsonSchema".to_string()));
    details.push("schemaRulesNotSatisfied".to_string(), BsonValue::Array(rules));
    let mut info = BsonDocument::new();
    info.push("failingDocumentId".to_string(), document.get("_id").cloned().unwrap_or(BsonValue::Null));
    info.push("details".to_string(), BsonValue::Document(details));
    info
}

/// Insert-path validation: `off` and `warn` never fail a write.
pub(crate) fn insert_validation_failure(
    store: &dyn MongoStorage,
    namespace: &str,
    document: &BsonDocument,
) -> Result<Option<BsonDocument>, CommandError> {
    let Some((schema, level, action)) = collection_schema(store, namespace)?
    else {
        return Ok(None);
    };
    if level == "off" || action == "warn" {
        return Ok(None);
    }
    let rules = schema_rules(&schema, document);
    if rules.is_empty() {
        return Ok(None);
    }
    Ok(Some(validation_err_info(document, rules)))
}

/// Update-path validation: `moderate` skips documents that were already
/// invalid before the update, matching the server.
pub(crate) fn update_validation_failure(
    store: &dyn MongoStorage,
    namespace: &str,
    original: &BsonDocument,
    updated: &BsonDocument,
) -> Result<Option<BsonDocument>, CommandError> {
    let Some((schema, level, action)) = collection_schema(store, namespace)?
    else {
        return Ok(None);
    };
    if level == "off" || action == "warn" {
        return Ok(None);
    }
    if level == "moderate" {
        let before = schema_rules(&schema, original);
        if !before.is_empty() {
            return Ok(None);
        }
    }
    let rules = schema_rules(&schema, updated);
    if rules.is_empty() {
        return Ok(None);
    }
    Ok(Some(validation_err_info(updated, rules)))
}

pub(crate) fn validation_options(command: &BsonDocument) -> Result<CollectionOptions, CommandError> {
    let mut options = CollectionOptions::default();
    if let Some(validator) = command.get("validator") {
        let BsonValue::Document(validator) = validator
        else {
            return Err(CommandError::new(2, "The 'validator' field must be an object".to_string()));
        };
        options.validator = Some(json::to_json(&BsonValue::Document(validator.clone())));
    }
    if let Some(level) = command.get("validationLevel") {
        let BsonValue::String(level) = level
        else {
            return Err(CommandError::new(2, "The 'validationLevel' field must be a string".to_string()));
        };
        if !matches!(level.as_str(), "off" | "strict" | "moderate") {
            return Err(CommandError::new(2, format!("Enumeration value '{level}' for field 'validationLevel' is not a valid value")));
        }
        options.validation_level = Some(level.clone());
    }
    if let Some(action) = command.get("validationAction") {
        let BsonValue::String(action) = action
        else {
            return Err(CommandError::new(2, "The 'validationAction' field must be a string".to_string()));
        };
        if !matches!(action.as_str(), "error" | "warn") {
            return Err(CommandError::new(2, format!("Enumeration value '{action}' for field 'validationAction' is not a valid value")));
        }
        options.validation_action = Some(action.clone());
    }
    Ok(options)
}

/// `create` with an optional validator, validation level, and action.
pub fn create(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let Some(collection) = command.get("create").and_then(|value| match value {
        BsonValue::String(name) => Some(name.clone()),
        _ => None,
    })
    else {
        return Err(CommandError::new(73, "create command missing collection name"));
    };
    let namespace = format!("{}.{collection}", command.database());
    let options = validation_options(command)?;
    // `mongod 8.0.9` treats `create` as idempotent when the stored options
    // match, and refuses only when they differ.
    if store.namespace_exists(&namespace)? {
        let existing = store.collection_options(&namespace)?;
        if existing != options {
            return Err(CommandError::new(48, create_conflict_message(&namespace, &existing)));
        }
        let mut response = BsonDocument::new();
        response.push("ok".to_string(), BsonValue::Double(1.0));
        return Ok(response);
    }
    store.create_collection(&namespace, options).map_err(|error| CommandError::new(2, format!("create failed: {error}")))?;
    if store.oplog_enabled() {
        let mut object = BsonDocument::new();
        object.push("create".to_string(), BsonValue::String(collection.clone()));
        for (field, value) in &command.fields {
            if field == "create" || is_control_field(field) {
                continue;
            }
            object.push(field.clone(), value.clone());
        }
        let mut id_index = BsonDocument::new();
        id_index.push("v".to_string(), BsonValue::Int32(2));
        let mut id_key = BsonDocument::new();
        id_key.push("_id".to_string(), BsonValue::Int32(1));
        id_index.push("key".to_string(), BsonValue::Document(id_key));
        id_index.push("name".to_string(), BsonValue::String("_id_".to_string()));
        object.push("idIndex".to_string(), BsonValue::Document(id_index));
        store.stage_oplog(OplogEntry::command(&command_namespace(&namespace), Some(collection_uuid(&namespace)), object, None));
    }
    let mut response = BsonDocument::new();
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// Command fields that describe the request rather than the write.
pub(crate) fn is_control_field(field: &str) -> bool {
    matches!(field, "$db" | "lsid" | "writeConcern" | "$clusterTime" | "apiVersion" | "comment" | "txnNumber" | "$readPreference")
}

/// `drop` removing one collection, its documents and its user indexes.
///
/// `mongod 8.0.9` answers `{nIndexesWas, ns, ok}` for an existing collection
/// and `{ok}` for a missing one; a view reply has no `nIndexesWas`, but this
/// server has no views, so that branch does not exist here.
pub fn drop(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let database = command.database();
    let collection = match command.get("drop") {
        Some(BsonValue::String(name)) => name.clone(),
        Some(other) => {
            return Err(CommandError::new(14, format!("collection name has invalid type {}", bson_type_name(other))));
        }
        None => return Err(CommandError::new(14, "collection name has invalid type int")),
    };
    if collection.is_empty() {
        return Err(CommandError::new(73, format!("Invalid namespace specified: {database}")));
    }
    let namespace = format!("{database}.{collection}");
    if store.oplog_enabled() && store.oplog_namespace().as_deref() == Some(namespace.as_str()) {
        return Err(CommandError::new(5255000, "can't drop live oplog while replicating"));
    }
    if matches!(database.as_str(), "admin" | "local" | "config") && collection.starts_with("system.") {
        return Err(CommandError::new(20, format!("cannot drop system collection {namespace}")));
    }
    if !store.namespace_exists(&namespace)? {
        let mut response = BsonDocument::new();
        response.push("ok".to_string(), BsonValue::Double(1.0));
        return Ok(response);
    }
    let records = store.scan(&namespace)?.len() as i32;
    let specifications = store.index_specs(&namespace)?;
    let user_indexes = specifications.len() as i32;
    if store.oplog_enabled() {
        let mut object = BsonDocument::new();
        object.push("drop".to_string(), BsonValue::String(collection.clone()));
        let mut old = BsonDocument::new();
        old.push("numRecords".to_string(), BsonValue::Int32(records));
        store.stage_oplog(OplogEntry::command(&command_namespace(&namespace), Some(collection_uuid(&namespace)), object, Some(old)));
    }
    for specification in &specifications {
        store
            .clear_reserved(&crate::mongo::storage::index_namespace(&namespace, &specification.name))
            .map_err(|error| CommandError::new(2, format!("drop failed: {error}")))?;
    }
    store.drop_namespace(&namespace).map_err(|error| CommandError::new(2, format!("drop failed: {error}")))?;
    let mut response = BsonDocument::new();
    response.push("nIndexesWas".to_string(), BsonValue::Int32(1 + user_indexes));
    response.push("ns".to_string(), BsonValue::String(namespace.clone()));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// `collMod` updating validator, validation level, and validation action in
/// place.
pub fn coll_mod(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let Some(collection) = command.get("collMod").and_then(|value| match value {
        BsonValue::String(name) => Some(name.clone()),
        _ => None,
    })
    else {
        return Err(CommandError::new(73, "collMod command missing collection name"));
    };
    let namespace = format!("{}.{collection}", command.database());
    if !store.namespace_exists(&namespace)? {
        return Err(CommandError::new(26, "ns does not exist".to_string()));
    }
    let mut options = store.collection_options(&namespace)?;
    let changes = validation_options(command)?;
    if changes.validator.is_some() {
        options.validator = changes.validator;
    }
    if changes.validation_level.is_some() {
        options.validation_level = changes.validation_level;
    }
    if changes.validation_action.is_some() {
        options.validation_action = changes.validation_action;
    }
    store.set_collection_options(&namespace, options).map_err(|error| CommandError::new(2, format!("collMod failed: {error}")))?;
    if store.oplog_enabled() {
        let mut object = BsonDocument::new();
        object.push("collMod".to_string(), BsonValue::String(collection.clone()));
        for (field, value) in &command.fields {
            if field == "collMod" || is_control_field(field) {
                continue;
            }
            object.push(field.clone(), value.clone());
        }
        let mut old = BsonDocument::new();
        let mut old_options = BsonDocument::new();
        old_options.push("uuid".to_string(), collection_uuid(&namespace));
        old.push("collectionOptions_old".to_string(), BsonValue::Document(old_options));
        store.stage_oplog(OplogEntry::command(&command_namespace(&namespace), Some(collection_uuid(&namespace)), object, Some(old)));
    }
    let mut response = BsonDocument::new();
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// `listDatabases`, derived from the stored namespaces.
pub fn list_databases(store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let mut names: Vec<String> =
        store.namespaces()?.iter().filter_map(|namespace| namespace.split_once('.').map(|(db, _)| db.to_string())).collect();
    names.sort();
    names.dedup();

    let databases: Vec<BsonValue> = names
        .into_iter()
        .map(|name| {
            let mut entry = BsonDocument::new();
            entry.push("name".to_string(), BsonValue::String(name));
            entry.push("sizeOnDisk".to_string(), BsonValue::Int64(0));
            entry.push("empty".to_string(), BsonValue::Bool(false));
            BsonValue::Document(entry)
        })
        .collect();

    let mut response = BsonDocument::new();
    response.push("databases".to_string(), BsonValue::Array(databases));
    response.push("totalSize".to_string(), BsonValue::Int64(0));
    response.push("totalSizeMb".to_string(), BsonValue::Int64(0));
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// `dropDatabase` reply.
///
/// `mongod 8.0.9` answers `{ ok: 1 }` and nothing else; the `dropped` field
/// printed by `mongosh`'s `db.dropDatabase()` helper is added client-side.
/// The command drops every collection of the target database together with its
/// indexes, collection options and the stored users, and the store guarantees a
/// failure leaves the database either fully present or fully absent.
pub fn drop_database(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let value = match command.get("dropDatabase") {
        None | Some(BsonValue::Null) => {
            return Err(CommandError::new(40414, "BSON field 'dropDatabase.dropDatabase' is missing but a required field"));
        }
        Some(value) => value,
    };
    let accepted = match value {
        BsonValue::Int32(number) => *number == 1,
        BsonValue::Int64(number) => *number == 1,
        // `mongod` accepts any double inside `[1, 2)` and rejects 2.0.
        BsonValue::Double(number) => (1.0..2.0).contains(number),
        BsonValue::Decimal128(number) => number.as_str().parse::<f64>().map(|value| value == 1.0).unwrap_or(false),
        other => {
            return Err(CommandError::new(
                14,
                format!(
                    "BSON field 'dropDatabase.dropDatabase' is the wrong type '{}', expected types '[int, double, long, decimal']",
                    bson_type_name(other)
                ),
            ));
        }
    };
    if !accepted {
        return Err(CommandError::new(5255100, "Have to pass 1 as 'drop' parameter"));
    }
    let database = command.database();
    if database == "admin" {
        return Err(CommandError::new(20, "Dropping the 'admin' database is prohibited."));
    }
    let prefix = format!("{database}.");
    let namespaces: Vec<String> = store.namespaces()?.into_iter().filter(|namespace| namespace.starts_with(&prefix)).collect();
    let had_namespaces = !namespaces.is_empty();
    for namespace in &namespaces {
        for specification in store.index_specs(namespace)? {
            store
                .clear_reserved(&crate::mongo::storage::index_namespace(namespace, &specification.name))
                .map_err(|error| CommandError::new(2, format!("dropDatabase failed: {error}")))?;
        }
    }
    if had_namespaces && store.oplog_enabled() {
        let mut object = BsonDocument::new();
        object.push("dropDatabase".to_string(), BsonValue::Int32(1));
        store.stage_oplog(OplogEntry::database_command(&format!("{database}.$cmd"), object));
    }
    store.drop_database(&database).map_err(|error| CommandError::new(2, format!("dropDatabase failed: {error}")))?;
    let mut response = BsonDocument::new();
    response.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(response)
}

/// `dbStats` reply, computed from the documents and keys the shard really holds.
///
/// `objects` counts the stored documents, `dataSize` sums their encoded BSON
/// lengths and `avgObjSize` is `dataSize / objects`; all three are exact.
/// `storageSize` is the byte count of the records this database owns in the
/// `.yykv` shard (key plus payload). `indexSize` sums the per-index footprints
/// `collStats` reports (definitions plus key material), so the two commands and
/// `collection.stats()` cannot disagree.
///
/// `filesystem` carries host-measured `(used, total)` bytes for the data
/// directory; `None` means the database does not exist, which is the case where
/// `mongod 8.0.9` also reports zero.
pub fn db_stats(command: &BsonDocument, store: &dyn MongoStorage, filesystem: Option<(i64, i64)>) -> Result<BsonDocument, CommandError> {
    let scale = db_stats_scale(command)?;
    let database = command.database();
    let prefix = format!("{database}.");

    let mut collections = 0_i64;
    let mut objects = 0_i64;
    let mut data_size = 0_i64;
    let mut storage_size = 0_i64;
    let mut index_size = 0_i64;
    let mut indexes = 0_i64;
    for namespace in store.namespaces()? {
        let Some(collection) = namespace.strip_prefix(&prefix)
        else {
            continue;
        };
        // `mongod 8.0.9` leaves `system.*` collections out of `dbStats`.
        if collection.starts_with("system.") {
            continue;
        }
        collections += 1;
        let sizes = collection_index_sizes(store, &namespace)?;
        indexes += sizes.len() as i64;
        index_size += sizes.iter().map(|(_, bytes)| *bytes).sum::<i64>();
        for (key, value) in store.scan(&namespace)? {
            objects += 1;
            data_size += value.len() as i64;
            storage_size += (key.len() + value.len()) as i64;
        }
    }

    let scaled = |bytes: i64| -> BsonValue { if scale == 1 { count_value(bytes) } else { count_value(bytes / scale) } };
    let mut document = BsonDocument::new();
    document.push("db".to_string(), BsonValue::String(database));
    document.push("collections".to_string(), count_value(collections));
    document.push("views".to_string(), BsonValue::Int32(0));
    document.push("objects".to_string(), count_value(objects));
    document.push(
        "avgObjSize".to_string(),
        if objects == 0 {
            BsonValue::Int32(0)
        }
        else if data_size % objects == 0 {
            count_value(data_size / objects)
        }
        else {
            BsonValue::Double(data_size as f64 / objects as f64)
        },
    );
    document.push("dataSize".to_string(), if scale == 1 { count_value(data_size) } else { BsonValue::Double(data_size as f64 / scale as f64) });
    document.push("storageSize".to_string(), scaled(storage_size));
    document.push("indexes".to_string(), count_value(indexes));
    document.push("indexSize".to_string(), scaled(index_size));
    document.push("totalSize".to_string(), scaled(storage_size + index_size));
    document.push("scaleFactor".to_string(), count_value(scale));
    let (used, total) = filesystem.unwrap_or((0, 0));
    document.push("fsUsedSize".to_string(), filesystem_value(used, scale));
    document.push("fsTotalSize".to_string(), filesystem_value(total, scale));
    document.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(document)
}

/// `scale` of `dbStats`, validated the way the real command does.
///
/// A missing field means factor 1, a factor below 1 is `BadValue`, and a
/// fractional double is ignored (`mongod 8.0.9` reports `scaleFactor: 1` for
/// `scale: 1.5` instead of rejecting it). Non-numeric values are `TypeMismatch`.
pub(crate) fn db_stats_scale(command: &BsonDocument) -> Result<i64, CommandError> {
    let Some(value) = command.get("scale")
    else {
        return Ok(1);
    };
    let scale = match value {
        BsonValue::Int32(number) => i64::from(*number),
        BsonValue::Int64(number) => *number,
        BsonValue::Double(number) => integral_value(*number).unwrap_or(1),
        BsonValue::Decimal128(number) => number.as_str().parse::<f64>().ok().and_then(integral_value).unwrap_or(1),
        other => {
            return Err(CommandError::new(
                14,
                format!(
                    "BSON field 'dbStats.scale' is the wrong type '{}', expected types '[decimal, double, int, long']",
                    bson_type_name(other)
                ),
            ));
        }
    };
    if scale < 1 {
        return Err(CommandError::new(2, "Scale factor must be greater than zero"));
    }
    Ok(scale)
}

/// Filesystem bytes keep `mongod`'s `long` type at factor 1 and shrink with it otherwise.
pub(crate) fn filesystem_value(bytes: i64, scale: i64) -> BsonValue {
    if scale == 1 { BsonValue::Int64(bytes) } else { count_value(bytes / scale) }
}

/// `collStats`, the per-collection half of `dbStats`.
///
/// The counts and bytes come from the same walk `dbStats` does, and
/// `indexSizes` comes from `collection_index_sizes`, so `dbStats.indexSize` is
/// exactly the sum of the `totalIndexSize` values this command reports.
///
/// `mongod 8.0.9` reports a missing collection with zero counts and empty
/// `indexSizes`; the storage-engine fields (`wiredTiger`, `indexDetails`,
/// `indexBuilds`) do not exist for `.yykv` and are omitted.
pub fn coll_stats(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let scale = coll_stats_scale(command)?;
    let collection = match command.get("collStats") {
        Some(BsonValue::String(name)) => name.clone(),
        Some(other) => {
            return Err(CommandError::new(14, format!("collection name has invalid type {}", bson_type_name(other))));
        }
        None => return Err(CommandError::new(14, "collection name has invalid type int")),
    };
    let namespace = format!("{}.{collection}", command.database());
    let scaled = |bytes: i64| -> BsonValue { if scale == 1 { count_value(bytes) } else { count_value(bytes / scale) } };

    let mut document = BsonDocument::new();
    document.push("ns".to_string(), BsonValue::String(namespace.clone()));
    if !store.namespace_exists(&namespace)? {
        document.push("size".to_string(), count_value(0));
        document.push("count".to_string(), count_value(0));
        document.push("numOrphanDocs".to_string(), count_value(0));
        document.push("storageSize".to_string(), count_value(0));
        document.push("totalSize".to_string(), count_value(0));
        document.push("nindexes".to_string(), count_value(0));
        document.push("totalIndexSize".to_string(), count_value(0));
        document.push("indexSizes".to_string(), BsonValue::Document(BsonDocument::new()));
        document.push("scaleFactor".to_string(), count_value(scale));
        document.push("ok".to_string(), BsonValue::Double(1.0));
        return Ok(document);
    }

    let mut count = 0_i64;
    let mut data_size = 0_i64;
    let mut storage_size = 0_i64;
    for (key, value) in store.scan(&namespace)? {
        count += 1;
        data_size += value.len() as i64;
        storage_size += (key.len() + value.len()) as i64;
    }
    let sizes = collection_index_sizes(store, &namespace)?;
    let total_index_size: i64 = sizes.iter().map(|(_, bytes)| *bytes).sum();

    document.push("size".to_string(), scaled(data_size));
    document.push("count".to_string(), count_value(count));
    if count > 0 {
        // `mongod 8.0.9` truncates this one to an integer, unlike `dbStats`.
        document.push("avgObjSize".to_string(), count_value(data_size / count));
    }
    document.push("numOrphanDocs".to_string(), count_value(0));
    document.push("storageSize".to_string(), scaled(storage_size));
    document.push("freeStorageSize".to_string(), scaled(0));
    document.push("capped".to_string(), BsonValue::Bool(false));
    document.push("nindexes".to_string(), count_value(sizes.len() as i64));
    document.push("totalIndexSize".to_string(), scaled(total_index_size));
    let mut index_sizes = BsonDocument::new();
    for (name, bytes) in &sizes {
        index_sizes.push(name.clone(), scaled(*bytes));
    }
    document.push("indexSizes".to_string(), BsonValue::Document(index_sizes));
    document.push("totalSize".to_string(), scaled(storage_size + total_index_size));
    document.push("scaleFactor".to_string(), count_value(scale));
    document.push("ok".to_string(), BsonValue::Double(1.0));
    Ok(document)
}

/// `collStats` scale, with the wording `mongod 8.0.9` uses for this command.
///
/// A missing field means factor 1, a fractional double is ignored, a factor
/// below 1 is `BadValue` and a non-numeric value is `TypeMismatch`.
pub(crate) fn coll_stats_scale(command: &BsonDocument) -> Result<i64, CommandError> {
    let Some(value) = command.get("scale")
    else {
        return Ok(1);
    };
    let scale = match value {
        BsonValue::Int32(number) => i64::from(*number),
        BsonValue::Int64(number) => *number,
        BsonValue::Double(number) => integral_value(*number).unwrap_or(1),
        BsonValue::Decimal128(number) => number.as_str().parse::<f64>().ok().and_then(integral_value).unwrap_or(1),
        other => {
            return Err(CommandError::new(
                14,
                format!(
                    "BSON field 'collStats.scale' is the wrong type '{}', expected types '[decimal, double, int, long']",
                    bson_type_name(other)
                ),
            ));
        }
    };
    if scale < 1 {
        return Err(CommandError::new(2, format!("BSON field 'scale' value must be >= 1, actual value '{scale}'")));
    }
    Ok(scale)
}
