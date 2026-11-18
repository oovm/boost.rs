//! 聚合管道：`aggregate` 入口与各阶段执行（unwind / lookup / group / merge 等）。

use super::*;

/// `aggregate` with the documented stages this gateway implements.
///
/// Every other stage answers `59`.
pub fn aggregate(command: &BsonDocument, store: &dyn MongoStorage) -> Result<BsonDocument, CommandError> {
    let namespace =
        namespace_from_command(command, "aggregate").ok_or_else(|| CommandError::new(73, "aggregate command missing collection name"))?;
    let session = SessionContext::new(command, store)?;

    let stages = match command.get("pipeline") {
        Some(BsonValue::Array(stages)) => stages,
        _ => return Err(CommandError::new(2, "aggregate requires a pipeline array")),
    };
    // 先成计划再执行：stage 形态、顺序与支持面在一次校验里定下来。
    let planned = plan_pipeline(stages)?;

    // 首个 `$match` 直接下推到访问路径：能走索引就走索引，后接 `$limit` 时还能
    // 提前停。`let` 绑定的变量只通过 `$$名称` 读取：`$match` 里没有 `$$` 引用时
    // 语义与无 `let` 完全一致，仍然下推；有引用就留在管道里按绑定求值。
    let documents;
    let mut remaining: &[PipelineStage] = &planned;
    if let Some(stage) = planned.first()
        && stage.name == "$match"
        && let BsonValue::Document(filter) = &stage.body
        && (command.get("let").is_none() || !references_variables(&stage.body))
    {
        validate_filter(filter, "aggregate $match")?;
        let stop_after = aggregate_stop_after(&planned[1..]);
        let access = choose_access(store, &namespace, filter, &Hint::None, &[], &[], session.key())?;
        documents = scan_with_access(store, &namespace, filter, &access, session.key(), false, stop_after, 0)?.documents;
        remaining = &planned[1..];
    }
    else {
        documents = collect_matching(store, &namespace, &BsonDocument::new(), session.key())?;
    }
    let documents = run_planned_pipeline(documents, remaining, store, &namespace, session.key(), &empty_variables())?;

    Ok(cursor_reply(&namespace, "firstBatch", documents.into_iter().map(BsonValue::Document).collect(), 0))
}

/// Applies one pipeline to an in-memory document set.
///
/// `$lookup` sub-pipelines and `$facet` branches call this recursively, so the
/// stage implementations never carry their own execution loop.
pub(crate) fn run_pipeline(
    documents: Vec<BsonDocument>,
    stages: &[BsonValue],
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let planned = plan_pipeline(stages)?;
    run_planned_pipeline(documents, &planned, store, namespace, session, variables)
}

/// Runs an already validated pipeline plan.
pub(crate) fn run_planned_pipeline(
    mut documents: Vec<BsonDocument>,
    stages: &[PipelineStage],
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    for stage in stages {
        let PipelineStage { name, body } = stage;
        let name = name.as_str();
        let body = body;
        // Each stage consumes the previous stage's output: the pipeline is applied
        // in order, never collected and matched in one pass.
        match name {
            "$match" => {
                let BsonValue::Document(filter) = body
                else {
                    return Err(CommandError::new(2, "$match requires a document"));
                };
                validate_filter(filter, "aggregate $match")?;
                let mut kept = Vec::new();
                for candidate in documents {
                    if document_matches_vars(&candidate, filter, variables)? {
                        kept.push(candidate);
                    }
                }
                documents = kept;
            }
            "$sort" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$sort requires a document"));
                };
                let sort = parse_sort_stage(specification)?;
                sort_documents(&mut documents, &sort);
            }
            "$limit" => {
                let limit = positive_stage_integer("$limit", body)?;
                documents.truncate(limit);
            }
            "$skip" => {
                let skip = positive_stage_integer("$skip", body)?;
                documents = documents.into_iter().skip(skip).collect();
            }
            "$count" => {
                let BsonValue::String(name) = body
                else {
                    return Err(CommandError::new(2, "$count requires a field name string"));
                };
                validate_count_name(name)?;
                // "If the input dataset is empty, $count doesn't return a result."
                if documents.is_empty() {
                    documents.clear();
                }
                else {
                    let mut counted = BsonDocument::new();
                    counted.push(name.clone(), count_value(documents.len() as i64));
                    documents = vec![counted];
                }
            }
            "$unwind" => documents = unwind_stage(&documents, body)?,
            "$lookup" => documents = lookup_stage(documents, body, store, namespace, session, variables)?,
            "$facet" => documents = facet_stage(documents, body, store, namespace, session, variables)?,
            "$bucket" => documents = bucket_stage(&documents, body, variables)?,
            "$bucketAuto" => documents = bucket_auto_stage(&documents, body, variables)?,
            "$setWindowFields" => documents = set_window_fields_stage(&documents, body, variables)?,
            "$sortByCount" => documents = sort_by_count_stage(&documents, body)?,
            "$graphLookup" => documents = graph_lookup_stage(documents, body, store, namespace, session, variables)?,
            "$replaceWith" => documents = replace_with_stage(documents, body, variables)?,
            "$unionWith" => {
                documents = union_with_stage(documents, body, store, namespace, session, variables)?;
            }
            "$sample" => documents = sample_stage(documents, body)?,
            "$densify" => documents = densify_stage(documents, body, variables)?,
            "$fill" => documents = fill_stage(documents, body, variables)?,
            "$redact" => documents = redact_stage(documents, body, variables)?,
            "$documents" => {
                return Err(CommandError::new(73, "'$documents' can only be run with {aggregate: 1}"));
            }
            "$merge" => {
                documents = merge_stage(documents, body, store, namespace, session, variables)?;
            }
            "$out" => {
                documents = out_stage(documents, body, store, namespace, session)?;
            }
            "$set" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$set requires a document"));
                };
                documents = add_fields_stage(&documents, specification)?;
            }
            "$group" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$group requires a document"));
                };
                documents = group_stage(&documents, specification)?;
            }
            "$addFields" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$addFields requires a document"));
                };
                documents = add_fields_stage(&documents, specification)?;
            }
            "$project" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$project requires a document"));
                };
                documents = project_stage(&documents, specification)?;
            }
            other => return Err(unsupported_stage(other)),
        }
    }
    Ok(documents)
}

/// `$unwind`: string form drops null/missing/empty-array documents; the document
/// form adds `preserveNullAndEmptyArrays` and `includeArrayIndex`.
pub(crate) fn unwind_stage(documents: &[BsonDocument], body: &BsonValue) -> Result<Vec<BsonDocument>, CommandError> {
    let (path, preserve, index_field) = match body {
        BsonValue::String(path) => (unwind_path(path)?, false, None),
        BsonValue::Document(specification) => {
            let BsonValue::String(path) = specification.get("path").unwrap_or(&BsonValue::Null)
            else {
                return Err(CommandError::new(2, "$unwind requires a `path` string"));
            };
            let path = unwind_path(path)?;
            let preserve = match specification.get("preserveNullAndEmptyArrays") {
                None => false,
                Some(BsonValue::Bool(flag)) => *flag,
                Some(_) => return Err(CommandError::new(2, "$unwind `preserveNullAndEmptyArrays` must be a boolean")),
            };
            let index_field = match specification.get("includeArrayIndex") {
                None => None,
                Some(BsonValue::String(field)) if !field.is_empty() && !field.starts_with('$') => Some(field.clone()),
                Some(_) => {
                    return Err(CommandError::new(2, "$unwind `includeArrayIndex` must be a non-empty string that does not start with `$`"));
                }
            };
            (path, preserve, index_field)
        }
        _ => return Err(CommandError::new(2, "$unwind requires a path string or a document")),
    };

    let mut output = Vec::new();
    for document in documents {
        let value = lookup_field(document, &path).cloned();
        match value {
            Some(BsonValue::Array(items)) if !items.is_empty() => {
                for (index, item) in items.into_iter().enumerate() {
                    let mut expanded = document.clone();
                    set_field_path(&mut expanded, &path, item);
                    if let Some(field) = &index_field {
                        expanded.push(field.clone(), BsonValue::Int64(index as i64));
                    }
                    output.push(expanded);
                }
            }
            Some(BsonValue::Array(_)) => {
                // Empty array: preserved only with the flag, and then the path is omitted.
                if preserve {
                    let mut expanded = document.clone();
                    remove_field_path(&mut expanded, &path);
                    if let Some(field) = &index_field {
                        expanded.push(field.clone(), BsonValue::Null);
                    }
                    output.push(expanded);
                }
            }
            Some(BsonValue::Null) => {
                if preserve {
                    let mut expanded = document.clone();
                    if let Some(field) = &index_field {
                        expanded.push(field.clone(), BsonValue::Null);
                    }
                    output.push(expanded);
                }
            }
            None => {
                if preserve {
                    let mut expanded = document.clone();
                    if let Some(field) = &index_field {
                        expanded.push(field.clone(), BsonValue::Null);
                    }
                    output.push(expanded);
                }
            }
            Some(scalar) => {
                let mut expanded = document.clone();
                set_field_path(&mut expanded, &path, scalar);
                if let Some(field) = &index_field {
                    expanded.push(field.clone(), BsonValue::Null);
                }
                output.push(expanded);
            }
        }
    }
    Ok(output)
}

pub(crate) fn unwind_path(path: &str) -> Result<String, CommandError> {
    match path.strip_prefix('$') {
        Some(rest) if !rest.is_empty() => Ok(rest.to_string()),
        _ => Err(CommandError::new(2, "$unwind path must start with `$`")),
    }
}

/// `$lookup` equality form (`from` / `localField` / `foreignField` / `as`) and
/// the pipeline form (`let` variables read through `$$name`).
///
/// A local array is expanded element by element and the matching foreign
/// documents are unioned in storage order, which is what `mongod 8.0.9` returns
/// for the equality form. A sub-pipeline runs against the foreign collection
/// after the implicit equality match, exactly as the manual describes.
pub(crate) fn lookup_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(9, "$lookup requires a document"));
    };
    let from = match specification.get("from") {
        Some(BsonValue::String(name)) if !name.is_empty() => name.clone(),
        Some(BsonValue::String(_)) | None => {
            return Err(CommandError::new(9, "must specify 'pipeline' when 'from' is empty"));
        }
        Some(other) => {
            return Err(CommandError::new(9, format!("$lookup 'from' field must be a string, but found {}", bson_type_name(other))));
        }
    };
    let as_field = match specification.get("as") {
        Some(BsonValue::String(field)) if !field.is_empty() => field.clone(),
        Some(other) => {
            return Err(CommandError::new(
                9,
                format!("$lookup argument 'as' must be a string, found as: {}: {}", render_shell_value(other), bson_type_name(other)),
            ));
        }
        None => return Err(CommandError::new(9, "must specify 'as' field for a $lookup")),
    };
    let pipeline = match specification.get("pipeline") {
        Some(BsonValue::Array(stages)) => Some(stages.clone()),
        Some(_) => return Err(CommandError::new(9, "$lookup 'pipeline' field must be an array")),
        None => None,
    };
    let let_variables = match specification.get("let") {
        Some(BsonValue::Document(bindings)) => bindings.clone(),
        Some(_) => return Err(CommandError::new(9, "$lookup 'let' field must be a document")),
        None => BsonDocument::new(),
    };
    let local_field = match specification.get("localField") {
        Some(BsonValue::String(field)) => Some(field.clone()),
        Some(other) => {
            return Err(CommandError::new(
                9,
                format!(
                    "$lookup argument 'localField' must be a string, found localField: {}: {}",
                    render_shell_value(other),
                    bson_type_name(other)
                ),
            ));
        }
        None => None,
    };
    let foreign_field = match specification.get("foreignField") {
        Some(BsonValue::String(field)) => Some(field.clone()),
        Some(other) => {
            return Err(CommandError::new(
                9,
                format!(
                    "$lookup argument 'foreignField' must be a string, found foreignField: {}: {}",
                    render_shell_value(other),
                    bson_type_name(other)
                ),
            ));
        }
        None => None,
    };
    if pipeline.is_none() && (local_field.is_none() || foreign_field.is_none()) {
        return Err(CommandError::new(9, "$lookup requires either 'pipeline' or both 'localField' and 'foreignField' to be specified"));
    }

    let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
    let foreign_namespace = format!("{database}.{from}");
    let foreign_documents = collect_matching(store, &foreign_namespace, &BsonDocument::new(), session)?;

    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        let mut bound = variables.clone();
        for (name, expression) in &let_variables.fields {
            bound.insert(name.clone(), evaluate_expression_vars(&document, expression, variables)?);
        }

        let mut candidates = foreign_documents.clone();
        if let (Some(local_field), Some(foreign_field)) = (&local_field, &foreign_field) {
            let value = lookup_field(&document, local_field).cloned().unwrap_or(BsonValue::Null);
            let local_values: Vec<&BsonValue> = match &value {
                BsonValue::Array(items) if !items.is_empty() => items.iter().collect(),
                // An empty local array matches the same foreign values as a missing
                // or null local field does, which is what `mongod 8.0.9` returns.
                BsonValue::Array(_) => vec![&BsonValue::Null],
                other => vec![other],
            };
            let mut matched = Vec::new();
            for foreign_document in candidates.into_iter() {
                let fields = collect_field(&foreign_document, foreign_field);
                if local_values.iter().any(|local| equality_candidates(&fields, local)) {
                    matched.push(foreign_document);
                }
            }
            candidates = matched;
        }
        if let Some(stages) = &pipeline {
            reject_stage_outside(stages, 51047, "is not allowed within a $lookup's sub-pipeline")?;
            candidates = run_pipeline(candidates, stages, store, &foreign_namespace, session, &bound)?;
        }

        let joined: Vec<BsonValue> = candidates.into_iter().map(BsonValue::Document).collect();
        let mut expanded = document;
        set_field_path(&mut expanded, &as_field, BsonValue::Array(joined));
        output.push(expanded);
    }
    Ok(output)
}

/// `$graphLookup`: breadth-first traversal of `connectFromField` →
/// `connectToField` starting from `startWith`.
///
/// The result array order is not defined by `mongod 8.0.9`, so callers that
/// need a stable shape project `$size` or a single-element chain.
pub(crate) fn graph_lookup_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(40105, "$graphLookup requires a document"));
    };
    let required = ["from", "as", "startWith", "connectFromField", "connectToField"];
    if required.iter().any(|field| specification.get(field).is_none()) {
        return Err(CommandError::new(
            40105,
            "$graphLookup requires 'from', 'as', 'startWith', 'connectFromField', and 'connectToField' to be specified.",
        ));
    }
    let from = match specification.get("from") {
        Some(BsonValue::String(name)) if !name.is_empty() => name.clone(),
        Some(other) => {
            return Err(CommandError::new(40105, format!("$graphLookup 'from' must be a string, found {}", bson_type_name(other))));
        }
        None => unreachable!(),
    };
    let as_field = match specification.get("as") {
        Some(BsonValue::String(field)) if !field.is_empty() => field.clone(),
        Some(other) => {
            return Err(CommandError::new(40105, format!("$graphLookup 'as' must be a non-empty string, found {}", bson_type_name(other))));
        }
        None => unreachable!(),
    };
    let start_with = specification.get("startWith").cloned().unwrap_or(BsonValue::Null);
    let connect_from = match specification.get("connectFromField") {
        Some(BsonValue::String(field)) => field.clone(),
        Some(other) => {
            return Err(CommandError::new(40105, format!("$graphLookup 'connectFromField' must be a string, found {}", bson_type_name(other))));
        }
        None => unreachable!(),
    };
    let connect_to = match specification.get("connectToField") {
        Some(BsonValue::String(field)) => field.clone(),
        Some(other) => {
            return Err(CommandError::new(40105, format!("$graphLookup 'connectToField' must be a string, found {}", bson_type_name(other))));
        }
        None => unreachable!(),
    };
    let max_depth = match specification.get("maxDepth") {
        None => None,
        Some(BsonValue::Int32(depth)) => {
            if *depth < 0 {
                return Err(CommandError::new(40101, format!("maxDepth requires a nonnegative argument, found: {depth}")));
            }
            Some(i64::from(*depth))
        }
        Some(BsonValue::Int64(depth)) => {
            if *depth < 0 {
                return Err(CommandError::new(40101, format!("maxDepth requires a nonnegative argument, found: {depth}")));
            }
            Some(*depth)
        }
        Some(BsonValue::Double(depth)) => {
            if *depth < 0.0 {
                return Err(CommandError::new(40101, format!("maxDepth requires a nonnegative argument, found: {depth}")));
            }
            if depth.fract() != 0.0 {
                return Err(CommandError::new(40102, format!("maxDepth could not be represented as a long long: {}", *depth as i64)));
            }
            Some(*depth as i64)
        }
        Some(other) => {
            return Err(CommandError::new(40102, format!("maxDepth could not be represented as a long long: {}", render_shell_value(other))));
        }
    };
    let depth_field = match specification.get("depthField") {
        Some(BsonValue::String(field)) => Some(field.clone()),
        Some(other) => {
            return Err(CommandError::new(40105, format!("$graphLookup 'depthField' must be a string, found {}", bson_type_name(other))));
        }
        None => None,
    };
    let restrict = match specification.get("restrictSearchWithMatch") {
        Some(BsonValue::Document(filter)) => {
            if filter.fields.iter().any(|(field, _)| field == "$where") {
                return Err(CommandError::new(
                    2,
                    "Failed to parse 'restrictSearchWithMatch' option to $graphLookup :: caused by :: $where is not allowed in this context",
                ));
            }
            validate_filter(filter, "$graphLookup restrictSearchWithMatch")?;
            Some(filter.clone())
        }
        Some(other) => {
            return Err(CommandError::new(
                2,
                format!("$graphLookup 'restrictSearchWithMatch' must be an object, found {}", bson_type_name(other)),
            ));
        }
        None => None,
    };

    let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
    let foreign_namespace = format!("{database}.{from}");
    let foreign_documents = collect_matching(store, &foreign_namespace, &BsonDocument::new(), session)?;

    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        let start = evaluate_expression_vars(&document, &start_with, variables)?;
        let mut queue: Vec<(BsonValue, i64)> = match start {
            BsonValue::Array(items) => items.into_iter().map(|item| (item, 0)).collect(),
            BsonValue::Null => Vec::new(),
            other => vec![(other, 0)],
        };
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut collected: Vec<BsonDocument> = Vec::new();
        while let Some((value, depth)) = queue.first().cloned() {
            queue.remove(0);
            for candidate in &foreign_documents {
                let fields = collect_field(candidate, &connect_to);
                if !equality_candidates(&fields, &value) {
                    continue;
                }
                if let Some(filter) = &restrict
                    && !document_matches_vars(candidate, filter, variables)?
                {
                    continue;
                }
                let identity = candidate.get("_id").map(value_to_key).unwrap_or_else(|| encode_bson(candidate));
                if !seen.insert(identity) {
                    continue;
                }
                let mut result = candidate.clone();
                if let Some(field) = &depth_field {
                    set_field_path(&mut result, field, BsonValue::Int64(depth));
                }
                collected.push(result);
                if max_depth.is_some_and(|limit| depth >= limit) {
                    continue;
                }
                for next in collect_field(candidate, &connect_from) {
                    match next {
                        BsonValue::Array(items) => {
                            for item in items {
                                queue.push((item.clone(), depth + 1));
                            }
                        }
                        BsonValue::Null => {}
                        other => queue.push((other.clone(), depth + 1)),
                    }
                }
            }
        }
        let mut expanded = document;
        set_field_path(&mut expanded, &as_field, BsonValue::Array(collected.into_iter().map(BsonValue::Document).collect()));
        output.push(expanded);
    }
    Ok(output)
}

/// `$replaceWith`: the input document is replaced by the evaluated document.
pub(crate) fn replace_with_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        match evaluate_expression_optional_vars(&document, body, variables)? {
            Some(BsonValue::Document(replacement)) => output.push(replacement),
            Some(other) => {
                return Err(CommandError::new(
                    40228,
                    format!(
                        "PlanExecutor error during aggregation :: caused by :: 'replacement document'  must evaluate to an object, but resulting value was: {}. Type of resulting value: '{}'. Input document: {{}}",
                        render_shell_value(&other),
                        bson_type_name(&other)
                    ),
                ));
            }
            None => {
                return Err(CommandError::new(
                    40228,
                    "PlanExecutor error during aggregation :: caused by :: 'replacement document'  must evaluate to an object, but resulting value was: MISSING. Type of resulting value: 'missing'. Input document: {}",
                ));
            }
        }
    }
    Ok(output)
}

/// `$out` target namespace resolution, shared by the error shapes and by
/// `$merge`.
pub(crate) fn stage_target_namespace(body: &BsonValue, namespace: &str, stage: &str) -> Result<String, CommandError> {
    let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
    match body {
        BsonValue::String(collection) => {
            if collection.is_empty() {
                return Err(CommandError::new(73, format!("Invalid $out target namespace, {database}")));
            }
            Ok(format!("{database}.{collection}"))
        }
        BsonValue::Document(specification) => {
            let target_database = match specification.get("db") {
                Some(BsonValue::String(name)) => name.clone(),
                _ => {
                    return Err(CommandError::new(40414, format!("BSON field '{stage}.db' is missing but a required field")));
                }
            };
            let collection = match specification.get("coll") {
                Some(BsonValue::String(name)) => name.clone(),
                _ => {
                    return Err(CommandError::new(40414, format!("BSON field '{stage}.coll' is missing but a required field")));
                }
            };
            for (field, _) in &specification.fields {
                if field != "db" && field != "coll" {
                    return Err(CommandError::new(40415, format!("BSON field '{stage}.{field}' is an unknown field.")));
                }
            }
            if collection.is_empty() {
                return Err(CommandError::new(73, format!("Invalid $out target namespace, {target_database}")));
            }
            Ok(format!("{target_database}.{collection}"))
        }
        other => Err(CommandError::new(16990, format!("$out only supports a string or object argument, but found {}", bson_type_name(other)))),
    }
}

pub(crate) fn temporary_aggregation_namespace(namespace: &str) -> String {
    let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
    let mut state =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_nanos() as u64).unwrap_or(0x9e37_79b9_7f4a_7c15) | 1;
    let mut hex = String::with_capacity(32);
    for _ in 0..32 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        hex.push(char::from_digit((state & 0xf) as u32, 16).unwrap());
    }
    format!("{database}.tmp.agg_out.{}-{}-4{}-{}-{}", &hex[0..8], &hex[8..12], &hex[13..16], &hex[16..20], &hex[20..32])
}

pub(crate) fn out_duplicate_error(namespace: &str, index_name: &str, key_value: &BsonDocument) -> CommandError {
    let mut key_text = String::new();
    for (field, value) in &key_value.fields {
        if !key_text.is_empty() {
            key_text.push_str(", ");
        }
        key_text.push_str(field);
        key_text.push_str(": ");
        key_text.push_str(&render_shell_value(value));
    }
    CommandError::new(
        11000,
        format!(
            "PlanExecutor error during aggregation :: caused by :: E11000 duplicate key error collection: {namespace} index: {index_name} dup key: {{ {key_text} }}"
        ),
    )
    .with_detail("keyValue", BsonValue::Document(key_value.clone()))
}

pub(crate) fn reject_stage_outside(stages: &[BsonValue], code: i32, within: &str) -> Result<(), CommandError> {
    for stage in stages {
        let BsonValue::Document(document) = stage
        else {
            continue;
        };
        if let Some((name, _)) = document.fields.first()
            && matches!(name.as_str(), "$out" | "$merge")
        {
            return Err(CommandError::new(code, format!("{name} {within}")));
        }
    }
    Ok(())
}

/// `$unionWith`: appends the documents of another collection, optionally
/// filtered by a sub-pipeline. The sub-pipeline runs in the target collection's
/// namespace, exactly like `$lookup`'s `from`.
pub(crate) fn union_with_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let (collection, pipeline) = match body {
        BsonValue::String(collection) => (Some(collection.clone()), None),
        BsonValue::Document(specification) => {
            let mut collection = None;
            let mut pipeline = None;
            for (key, value) in &specification.fields {
                match key.as_str() {
                    "coll" => match value {
                        BsonValue::String(name) => collection = Some(name.clone()),
                        other => {
                            return Err(CommandError::new(
                                14,
                                format!("BSON field '$unionWith.coll' is the wrong type '{}', expected type 'string'", bson_type_name(other)),
                            ));
                        }
                    },
                    "pipeline" => match value {
                        BsonValue::Array(stages) => pipeline = Some(stages.clone()),
                        _ => return Err(CommandError::new(14, "'pipeline' option must be specified as an array")),
                    },
                    other => {
                        return Err(CommandError::new(40415, format!("BSON field '$unionWith.{other}' is an unknown field.")));
                    }
                }
            }
            (collection, pipeline)
        }
        other => {
            return Err(CommandError::new(
                9,
                format!("the $unionWith stage specification must be an object or string, but found {}", bson_type_name(other)),
            ));
        }
    };

    let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
    let documents_required =
        || CommandError::new(9, "$unionWith stage without explicit collection must have a pipeline with $documents as first stage");
    let mut output = documents;
    match collection {
        Some(collection) if !collection.is_empty() => {
            let target = format!("{database}.{collection}");
            let mut candidates = collect_matching(store, &target, &BsonDocument::new(), session)?;
            if let Some(stages) = &pipeline {
                candidates = run_pipeline(candidates, stages, store, &target, session, variables)?;
            }
            output.extend(candidates);
        }
        Some(_) => {
            return Err(CommandError::new(
                73,
                format!("PlanExecutor error during aggregation :: caused by :: Namespace {database} is not a valid collection name"),
            ));
        }
        None => {
            let Some(stages) = pipeline
            else {
                return Err(documents_required());
            };
            let Some(BsonValue::Document(first)) = stages.first()
            else {
                return Err(documents_required());
            };
            let Some((first_name, first_body)) = first.fields.first()
            else {
                return Err(documents_required());
            };
            if first_name != "$documents" || first.fields.len() != 1 {
                return Err(documents_required());
            }
            let BsonValue::Array(items) = first_body
            else {
                return Err(CommandError::new(
                    14,
                    format!("BSON field '$documents' is the wrong type '{}', expected type 'array'", bson_type_name(first_body)),
                ));
            };
            let mut candidates = Vec::with_capacity(items.len());
            for item in items {
                let BsonValue::Document(document) = item
                else {
                    return Err(CommandError::new(
                        14,
                        format!("$documents requires an array of documents, but found {}", bson_type_name(item)),
                    ));
                };
                candidates.push(document.clone());
            }
            if stages.len() > 1 {
                candidates = run_pipeline(candidates, &stages[1..], store, namespace, session, variables)?;
            }
            output.extend(candidates);
        }
    }
    Ok(output)
}

/// `$sample`: at most `size` documents in arbitrary order. `mongod` truncates a
/// fractional size toward zero and treats anything at or above the input count
/// as "all documents".
pub(crate) fn sample_stage(mut documents: Vec<BsonDocument>, body: &BsonValue) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(28745, "the $sample stage specification must be an object"));
    };
    for (key, _) in &specification.fields {
        if key != "size" {
            return Err(CommandError::new(28748, format!("unrecognized option to $sample: {key}")));
        }
    }
    let Some(value) = specification.get("size")
    else {
        return Err(CommandError::new(28749, "$sample stage must specify a size"));
    };
    let Some(number) = number_of(value)
    else {
        return Err(CommandError::new(28746, "size argument to $sample must be a number"));
    };
    let truncated = number.trunc();
    if truncated < 0.0 {
        return Err(CommandError::new(28747, "size argument to $sample must not be negative"));
    }
    let size = if truncated >= documents.len() as f64 { documents.len() } else { truncated as usize };

    // A xorshift shuffle keeps `$sample` random without pulling a dependency in.
    let mut state =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_nanos() as u64).unwrap_or(0x9e37_79b9_7f4a_7c15) | 1;
    for index in (1..documents.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let swap = (state % (index as u64 + 1)) as usize;
        documents.swap(index, swap);
    }
    documents.truncate(size);
    Ok(documents)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DensifyKind {
    Numeric,
    Date,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeUnit {
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl TimeUnit {
    fn millis(self) -> Option<i64> {
        match self {
            TimeUnit::Millisecond => Some(1),
            TimeUnit::Second => Some(1_000),
            TimeUnit::Minute => Some(60_000),
            TimeUnit::Hour => Some(3_600_000),
            TimeUnit::Day => Some(86_400_000),
            TimeUnit::Week => Some(604_800_000),
            TimeUnit::Month | TimeUnit::Year => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum DensifyBounds {
    Full,
    Partition,
    Array(BsonValue, BsonValue),
}

pub(crate) struct DensifySpecification {
    pub(crate) field: String,
    pub(crate) partition_fields: Vec<String>,
    pub(crate) bounds: DensifyBounds,
    pub(crate) step: f64,
    pub(crate) unit: Option<TimeUnit>,
}

pub(crate) fn parse_densify_unit(value: &BsonValue) -> Result<TimeUnit, CommandError> {
    let BsonValue::String(name) = value
    else {
        return Err(CommandError::new(
            14,
            format!("BSON field '$densify.range.unit' is the wrong type '{}', expected type 'string'", bson_type_name(value)),
        ));
    };
    match name.as_str() {
        "millisecond" | "milliseconds" => Ok(TimeUnit::Millisecond),
        "second" | "seconds" => Ok(TimeUnit::Second),
        "minute" | "minutes" => Ok(TimeUnit::Minute),
        "hour" | "hours" => Ok(TimeUnit::Hour),
        "day" | "days" => Ok(TimeUnit::Day),
        "week" | "weeks" => Ok(TimeUnit::Week),
        "month" | "months" => Ok(TimeUnit::Month),
        "year" | "years" => Ok(TimeUnit::Year),
        other => Err(CommandError::new(9, format!("unknown time unit value: {other}"))),
    }
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub(crate) fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            }
            else {
                28
            }
        }
    }
}

pub(crate) fn add_calendar_units(millis: i64, count: i64, unit: TimeUnit) -> i64 {
    let day_millis = 86_400_000;
    let remainder = millis.rem_euclid(day_millis);
    let days = (millis - remainder) / day_millis;
    let (year, month, day) = civil_from_days(days);
    let (new_year, new_month) = match unit {
        TimeUnit::Month => {
            let total = year * 12 + (month - 1) + count;
            (total.div_euclid(12), total.rem_euclid(12) + 1)
        }
        TimeUnit::Year => (year + count, month),
        _ => (year, month),
    };
    let clamped_day = day.min(days_in_month(new_year, new_month));
    days_from_civil(new_year, new_month, clamped_day) * day_millis + remainder
}

pub(crate) fn densify_value_as_number(value: &BsonValue) -> Option<f64> {
    match value {
        BsonValue::Int32(number) => Some(f64::from(*number)),
        BsonValue::Int64(number) => Some(*number as f64),
        BsonValue::Double(number) => Some(*number),
        _ => None,
    }
}

pub(crate) fn densify_number_value(value: f64, hint: &BsonValue) -> BsonValue {
    match hint {
        BsonValue::Int32(_) => {
            if let Ok(integer) = i32::try_from(value as i64)
                && value == integer as f64
            {
                return BsonValue::Int32(integer);
            }
            BsonValue::Double(value)
        }
        BsonValue::Int64(_) => {
            let integer = value as i64;
            if value == integer as f64 { BsonValue::Int64(integer) } else { BsonValue::Double(value) }
        }
        _ => BsonValue::Double(value),
    }
}

pub(crate) fn parse_densify(body: &BsonValue) -> Result<DensifySpecification, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(9, "the $densify stage specification must be an object"));
    };
    let mut field = None;
    let mut partition_fields = Vec::new();
    let mut range = None;
    for (key, value) in &specification.fields {
        match key.as_str() {
            "field" => match value {
                BsonValue::String(name) => field = Some(name.clone()),
                other => {
                    return Err(CommandError::new(
                        14,
                        format!("BSON field '$densify.field' is the wrong type '{}', expected type 'string'", bson_type_name(other)),
                    ));
                }
            },
            "partitionByFields" => match value {
                BsonValue::Array(items) => {
                    for item in items {
                        let BsonValue::String(name) = item
                        else {
                            return Err(CommandError::new(
                                14,
                                format!(
                                    "BSON field '$densify.partitionByFields' is the wrong type '{}', expected type 'string'",
                                    bson_type_name(item)
                                ),
                            ));
                        };
                        partition_fields.push(name.clone());
                    }
                }
                other => {
                    return Err(CommandError::new(
                        14,
                        format!("BSON field '$densify.partitionByFields' is the wrong type '{}', expected type 'array'", bson_type_name(other)),
                    ));
                }
            },
            "range" => range = Some(value),
            other => {
                return Err(CommandError::new(40415, format!("BSON field '$densify.{other}' is an unknown field.")));
            }
        }
    }
    let Some(field) = field
    else {
        return Err(CommandError::new(40414, "BSON field '$densify.field' is missing but a required field"));
    };
    if field.starts_with('$') {
        return Err(CommandError::new(16410, "FieldPath field names may not start with '$'. Consider using $getField or $setField."));
    }
    for name in &partition_fields {
        if name.starts_with('$') {
            return Err(CommandError::new(16410, "FieldPath field names may not start with '$'. Consider using $getField or $setField."));
        }
    }
    let Some(range) = range
    else {
        return Err(CommandError::new(40414, "BSON field '$densify.range' is missing but a required field"));
    };
    let BsonValue::Document(range) = range
    else {
        return Err(CommandError::new(
            14,
            format!("BSON field '$densify.range' is the wrong type '{}', expected type 'object'", bson_type_name(range)),
        ));
    };
    let mut step = None;
    let mut unit = None;
    let mut bounds = None;
    for (key, value) in &range.fields {
        match key.as_str() {
            "step" => match value {
                BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) => step = Some(number_of(value).unwrap_or(0.0)),
                other => {
                    return Err(CommandError::new(
                        14,
                        format!(
                            "BSON field '$densify.range.step' is the wrong type '{}', expected types '[int, double, decimal, long']",
                            bson_type_name(other)
                        ),
                    ));
                }
            },
            "unit" => unit = Some(parse_densify_unit(value)?),
            "bounds" => bounds = Some(value),
            other => {
                return Err(CommandError::new(40415, format!("BSON field '$densify.range.{other}' is an unknown field.")));
            }
        }
    }
    let Some(step) = step
    else {
        return Err(CommandError::new(40414, "BSON field '$densify.range.step' is missing but a required field"));
    };
    if step <= 0.0 {
        return Err(CommandError::new(5733401, "The step parameter in a range statement must be a strictly positive numeric value"));
    }
    let Some(bounds) = bounds
    else {
        return Err(CommandError::new(40414, "BSON field '$densify.range.bounds' is missing but a required field"));
    };
    let bounds = match bounds {
        BsonValue::String(name) => match name.as_str() {
            "full" => DensifyBounds::Full,
            "partition" => {
                if partition_fields.is_empty() {
                    return Err(CommandError::new(
                        5733408,
                        "One cannot specify the bounds as 'partition' without specifying a non-empty array of partitionByFields. You may have meant to specify 'full' bounds.",
                    ));
                }
                DensifyBounds::Partition
            }
            _ => return Err(CommandError::new(5946802, "Bounds string must either be 'full' or 'partition'")),
        },
        BsonValue::Array(items) if items.len() == 2 => {
            let lower = &items[0];
            let upper = &items[1];
            let numeric = densify_value_as_number(lower).is_some() && densify_value_as_number(upper).is_some();
            let dated = matches!(lower, BsonValue::DateTime(_)) && matches!(upper, BsonValue::DateTime(_));
            if numeric {
                if unit.is_some() {
                    return Err(CommandError::new(5733409, "Numeric bounds may not have unit parameter"));
                }
                if densify_value_as_number(lower).unwrap() >= densify_value_as_number(upper).unwrap() {
                    return Err(CommandError::new(5733402, "A bounding array must be an ascending array of either two dates or two numbers"));
                }
            }
            else if !dated || unit.is_none() {
                return Err(CommandError::new(5733402, "A bounding array must be an ascending array of either two dates or two numbers"));
            }
            else if let (BsonValue::DateTime(lower), BsonValue::DateTime(upper)) = (lower, upper)
                && *lower >= *upper
            {
                return Err(CommandError::new(5733402, "A bounding array must be an ascending array of either two dates or two numbers"));
            }
            DensifyBounds::Array(lower.clone(), upper.clone())
        }
        _ => {
            return Err(CommandError::new(5733402, "A bounding array must be an ascending array of either two dates or two numbers"));
        }
    };
    Ok(DensifySpecification { field, partition_fields, bounds, step, unit })
}

pub(crate) fn densify_partition_fields(specification: &DensifySpecification, document: &BsonDocument) -> Vec<BsonValue> {
    specification.partition_fields.iter().map(|field| lookup_field(document, field).cloned().unwrap_or(BsonValue::Null)).collect()
}

pub(crate) fn densify_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let specification = parse_densify(body)?;
    let _ = variables;

    let mut kind: Option<DensifyKind> = None;
    for document in &documents {
        let Some(value) = lookup_field(document, &specification.field)
        else {
            continue;
        };
        match value {
            BsonValue::Null | BsonValue::Undefined => {}
            BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) => {
                if kind == Some(DensifyKind::Date) {
                    return Err(CommandError::new(
                        5733201,
                        "PlanExecutor error during aggregation :: caused by :: Densify field type must be numeric or a date",
                    ));
                }
                kind = Some(DensifyKind::Numeric);
            }
            BsonValue::DateTime(_) => {
                if kind == Some(DensifyKind::Numeric) {
                    return Err(CommandError::new(
                        5733201,
                        "PlanExecutor error during aggregation :: caused by :: Densify field type must be numeric or a date",
                    ));
                }
                kind = Some(DensifyKind::Date);
            }
            _ => {
                return Err(CommandError::new(
                    5733201,
                    "PlanExecutor error during aggregation :: caused by :: Densify field type must be numeric or a date",
                ));
            }
        }
    }
    let Some(kind) = kind
    else {
        return Ok(documents);
    };
    if kind == DensifyKind::Date && specification.unit.is_none() {
        return Err(CommandError::new(
            6053600,
            "PlanExecutor error during aggregation :: caused by :: Encountered date densify value in collection when step does not have a date unit.",
        ));
    }
    if kind == DensifyKind::Numeric && specification.unit.is_some() {
        return Err(CommandError::new(
            6053600,
            "PlanExecutor error during aggregation :: caused by :: Encountered numeric densify value in collection when step has a date unit.",
        ));
    }

    // Sort by partition fields and then the densified field, which is the order
    // `mongod` emits (the gap documents are spliced into that sequence).
    let mut sorted = documents;
    sorted.sort_by(|left, right| {
        for (position, field) in specification.partition_fields.iter().enumerate() {
            let _ = position;
            let left_value = lookup_field(left, field).cloned().unwrap_or(BsonValue::Null);
            let right_value = lookup_field(right, field).cloned().unwrap_or(BsonValue::Null);
            let ordering = compare_values(&left_value, &right_value);
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        let left_value = lookup_field(left, &specification.field).cloned().unwrap_or(BsonValue::Null);
        let right_value = lookup_field(right, &specification.field).cloned().unwrap_or(BsonValue::Null);
        compare_values(&left_value, &right_value)
    });

    let numeric_value = |document: &BsonDocument| -> Option<f64> {
        lookup_field(document, &specification.field).and_then(|value| match value {
            BsonValue::Null | BsonValue::Undefined => None,
            other => densify_value_as_number(other),
        })
    };
    let date_value = |document: &BsonDocument| -> Option<i64> {
        lookup_field(document, &specification.field).and_then(|value| match value {
            BsonValue::DateTime(millis) => Some(*millis),
            _ => None,
        })
    };

    let mut global_range: Option<(f64, f64)> = None;
    if matches!(specification.bounds, DensifyBounds::Full) {
        let mut lower = f64::INFINITY;
        let mut upper = f64::NEG_INFINITY;
        for document in &sorted {
            if let Some(value) = numeric_value(document) {
                lower = lower.min(value);
                upper = upper.max(value);
            }
            else if let Some(millis) = date_value(document) {
                lower = lower.min(millis as f64);
                upper = upper.max(millis as f64);
            }
        }
        if lower.is_finite() && upper.is_finite() {
            global_range = Some((lower, upper));
        }
    }
    let mut global_dates: Option<(i64, i64)> = None;
    if matches!(specification.bounds, DensifyBounds::Full) {
        let mut lower = i64::MAX;
        let mut upper = i64::MIN;
        for document in &sorted {
            if let Some(millis) = date_value(document) {
                lower = lower.min(millis);
                upper = upper.max(millis);
            }
        }
        if lower != i64::MAX {
            global_dates = Some((lower, upper));
        }
    }

    let mut output: Vec<BsonDocument> = Vec::with_capacity(sorted.len());
    let mut index = 0;
    while index < sorted.len() {
        let key = densify_partition_fields(&specification, &sorted[index]);
        let start = index;
        while index < sorted.len() && densify_partition_fields(&specification, &sorted[index]) == key {
            index += 1;
        }
        let group = &sorted[start..index];

        let hint = group
            .iter()
            .find_map(|document| match lookup_field(document, &specification.field) {
                Some(value @ (BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_))) => Some(value.clone()),
                _ => None,
            })
            .unwrap_or(BsonValue::Double(0.0));

        let mut existing_numbers: BTreeSet<u64> = BTreeSet::new();
        let mut group_number_range: Option<(f64, f64)> = None;
        let mut existing_dates: BTreeSet<i64> = BTreeSet::new();
        let mut group_date_range: Option<(i64, i64)> = None;
        for document in group {
            if let Some(value) = numeric_value(document) {
                existing_numbers.insert(value.to_bits());
                group_number_range = Some(match group_number_range {
                    Some((lower, upper)) => (lower.min(value), upper.max(value)),
                    None => (value, value),
                });
            }
            if let Some(millis) = date_value(document) {
                existing_dates.insert(millis);
                group_date_range = Some(match group_date_range {
                    Some((lower, upper)) => (lower.min(millis), upper.max(millis)),
                    None => (millis, millis),
                });
            }
        }

        let mut generated: Vec<BsonDocument> = Vec::new();
        let make_document = |value: BsonValue| {
            let mut document = BsonDocument::new();
            for (field, field_value) in specification.partition_fields.iter().zip(&key) {
                let _ = field;
                if let Some(original) = group.iter().find_map(|candidate| lookup_field(candidate, field).cloned()) {
                    document.push(field.clone(), original);
                }
                else {
                    let _ = field_value;
                }
            }
            document.push(specification.field.clone(), value);
            document
        };

        if kind == DensifyKind::Numeric {
            let range = match &specification.bounds {
                DensifyBounds::Full => global_range,
                DensifyBounds::Partition => group_number_range,
                DensifyBounds::Array(lower, upper) => match (densify_value_as_number(lower), densify_value_as_number(upper)) {
                    (Some(lower), Some(upper)) => Some((lower, upper)),
                    _ => None,
                },
            };
            if let Some((lower, upper)) = range {
                let mut value = lower;
                let mut iterations = 0usize;
                while value < upper && iterations < 10_000_000 {
                    if !existing_numbers.contains(&value.to_bits()) {
                        generated.push(make_document(densify_number_value(value, &hint)));
                    }
                    value += specification.step;
                    iterations += 1;
                }
            }
        }
        else {
            let unit = specification.unit.unwrap_or(TimeUnit::Millisecond);
            let range = match &specification.bounds {
                DensifyBounds::Full => global_dates,
                DensifyBounds::Partition => group_date_range,
                DensifyBounds::Array(lower, upper) => match (lower, upper) {
                    (BsonValue::DateTime(lower), BsonValue::DateTime(upper)) => Some((*lower, *upper)),
                    _ => None,
                },
            };
            if let Some((lower, upper)) = range {
                let mut value = lower;
                let mut iterations = 0usize;
                while value < upper && iterations < 10_000_000 {
                    if !existing_dates.contains(&value) {
                        generated.push(make_document(BsonValue::DateTime(value)));
                    }
                    match unit {
                        TimeUnit::Month | TimeUnit::Year => {
                            value = add_calendar_units(value, 1, unit);
                        }
                        _ => {
                            value = value.saturating_add(unit.millis().unwrap_or(1));
                        }
                    }
                    iterations += 1;
                }
            }
        }

        // Merge the group's own documents with the generated gap documents.
        let mut group_cursor = 0;
        let mut generated_cursor = 0;
        while group_cursor < group.len() || generated_cursor < generated.len() {
            let take_original = match (group.get(group_cursor), generated.get(generated_cursor)) {
                (Some(original), Some(gap)) => {
                    let original_value = lookup_field(original, &specification.field).cloned().unwrap_or(BsonValue::Null);
                    let gap_value = lookup_field(gap, &specification.field).cloned().unwrap_or(BsonValue::Null);
                    compare_values(&original_value, &gap_value) != Ordering::Greater
                }
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_original {
                output.push(group[group_cursor].clone());
                group_cursor += 1;
            }
            else {
                output.push(generated[generated_cursor].clone());
                generated_cursor += 1;
            }
        }
    }
    Ok(output)
}

#[derive(Clone)]
pub(crate) enum FillMethod {
    Value(BsonValue),
    Locf,
    Linear,
}

pub(crate) struct FillField {
    pub(crate) field: String,
    pub(crate) method: FillMethod,
}

pub(crate) fn fill_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(9, "the $fill stage specification must be an object"));
    };
    let mut sort_by: Option<Vec<(String, i32)>> = None;
    let mut partition_by: Option<BsonValue> = None;
    let mut partition_fields: Option<Vec<String>> = None;
    let mut output = None;
    for (key, value) in &specification.fields {
        match key.as_str() {
            "sortBy" => match value {
                BsonValue::Document(sort) => sort_by = Some(parse_sort_stage(sort)?),
                other => {
                    return Err(CommandError::new(
                        14,
                        format!("BSON field '$fill.sortBy' is the wrong type '{}', expected type 'object'", bson_type_name(other)),
                    ));
                }
            },
            "partitionBy" => partition_by = Some(value.clone()),
            "partitionByFields" => match value {
                BsonValue::Array(items) => {
                    let mut fields = Vec::with_capacity(items.len());
                    for item in items {
                        let BsonValue::String(name) = item
                        else {
                            return Err(CommandError::new(
                                14,
                                format!(
                                    "BSON field '$fill.partitionByFields' is the wrong type '{}', expected type 'string'",
                                    bson_type_name(item)
                                ),
                            ));
                        };
                        fields.push(name.clone());
                    }
                    partition_fields = Some(fields);
                }
                other => {
                    return Err(CommandError::new(
                        14,
                        format!("BSON field '$fill.partitionByFields' is the wrong type '{}', expected type 'array'", bson_type_name(other)),
                    ));
                }
            },
            "output" => output = Some(value),
            other => {
                return Err(CommandError::new(40415, format!("BSON field '$fill.{other}' is an unknown field.")));
            }
        }
    }
    let Some(output) = output
    else {
        return Err(CommandError::new(40414, "BSON field '$fill.output' is missing but a required field"));
    };
    let BsonValue::Document(output) = output
    else {
        return Err(CommandError::new(
            14,
            format!("BSON field '$fill.output' is the wrong type '{}', expected type 'object'", bson_type_name(output)),
        ));
    };
    if output.fields.is_empty() {
        return Err(CommandError::new(6050203, "The output field in '$fill' must contain an object with at least one element"));
    }
    let mut fills: Vec<FillField> = Vec::new();
    for (field, entry) in &output.fields {
        let BsonValue::Document(entry) = entry
        else {
            return Err(CommandError::new(6050200, "Each fill output specification must be an object with exactly one field"));
        };
        for (name, _) in &entry.fields {
            if name != "value" && name != "method" {
                return Err(CommandError::new(40415, format!("BSON field '$fill.{name}' is an unknown field.")));
            }
        }
        let value = entry.get("value");
        let method = entry.get("method");
        if value.is_some() && method.is_some() {
            return Err(CommandError::new(
                6050201,
                "Each fill output specification must have exactly one of 'method' or 'value' fields, not both",
            ));
        }
        let fill_method = match (value, method) {
            (Some(value), None) => FillMethod::Value(value.clone()),
            (None, Some(method)) => match method {
                BsonValue::String(name) if name == "locf" => FillMethod::Locf,
                BsonValue::String(name) if name == "linear" => FillMethod::Linear,
                _ => return Err(CommandError::new(6050202, "Method must be either locf or linear")),
            },
            (None, None) => continue,
            _ => unreachable!(),
        };
        fills.push(FillField { field: field.clone(), method: fill_method });
    }
    if partition_by.is_some() && partition_fields.is_some() {
        return Err(CommandError::new(6050204, "Maximum one of 'partitionBy' and 'partitionByFields can be specified in '$fill'"));
    }
    let linear = fills.iter().any(|fill| matches!(fill.method, FillMethod::Linear));
    if linear {
        match &sort_by {
            Some(sort) if sort.len() == 1 => {}
            _ => {
                return Err(CommandError::new(
                    605001,
                    "$linearFill must be specified with a top level sortBy expression with exactly one element",
                ));
            }
        }
    }

    let partition_key = |document: &BsonDocument| -> Result<Vec<u8>, CommandError> {
        if let Some(fields) = &partition_fields {
            let mut key = Vec::new();
            for field in fields {
                let value = lookup_field(document, field).cloned().unwrap_or(BsonValue::Null);
                key.extend_from_slice(&value_to_key(&value));
            }
            return Ok(key);
        }
        if let Some(expression) = &partition_by {
            let value = evaluate_expression_vars(document, expression, variables)?;
            return Ok(value_to_key(&value));
        }
        Ok(Vec::new())
    };

    let mut working = documents;
    let ordered = fills.iter().any(|fill| !matches!(fill.method, FillMethod::Value(_)));
    if ordered && let Some(sort) = &sort_by {
        sort_documents(&mut working, sort);
    }

    // Group indices by partition while preserving the document order.
    let mut groups: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
    for (position, document) in working.iter().enumerate() {
        let key = partition_key(document)?;
        match groups.iter_mut().find(|(existing, _)| existing == &key) {
            Some((_, members)) => members.push(position),
            None => groups.push((key, vec![position])),
        }
    }

    for (_, members) in &groups {
        for fill in &fills {
            match &fill.method {
                FillMethod::Value(value) => {
                    for position in members {
                        let document = &mut working[*position];
                        match lookup_field(document, &fill.field) {
                            Some(BsonValue::Null) | Some(BsonValue::Undefined) | None => {
                                set_field_path(document, &fill.field, value.clone());
                            }
                            Some(_) => {}
                        }
                    }
                }
                FillMethod::Locf => {
                    let mut last: Option<BsonValue> = None;
                    for position in members {
                        let document = &mut working[*position];
                        match lookup_field(document, &fill.field).cloned() {
                            Some(BsonValue::Null) | Some(BsonValue::Undefined) | None => {
                                if let Some(value) = &last {
                                    set_field_path(document, &fill.field, value.clone());
                                }
                            }
                            Some(value) => last = Some(value),
                        }
                    }
                }
                FillMethod::Linear => {
                    let mut anchors: Vec<(usize, f64)> = Vec::new();
                    for position in members {
                        match lookup_field(&working[*position], &fill.field) {
                            Some(BsonValue::Null) | Some(BsonValue::Undefined) | None => {}
                            Some(BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_)) => {
                                let value = densify_value_as_number(lookup_field(&working[*position], &fill.field).unwrap()).unwrap();
                                anchors.push((*position, value));
                            }
                            Some(other) => {
                                return Err(CommandError::new(
                                    14,
                                    format!(
                                        "PlanExecutor error during aggregation :: caused by :: Value to be filled must be numeric or nullish, but found {}",
                                        bson_type_name(other)
                                    ),
                                ));
                            }
                        }
                    }
                    for pair in anchors.windows(2) {
                        let (left_position, left_value) = pair[0];
                        let (right_position, right_value) = pair[1];
                        if right_position == left_position + 1 {
                            continue;
                        }
                        let steps = (right_position - left_position) as f64;
                        for offset in 1..(right_position - left_position) {
                            let ratio = offset as f64 / steps;
                            let value = left_value + (right_value - left_value) * ratio;
                            // `mongod` always yields a double for interpolated values.
                            set_field_path(&mut working[left_position + offset], &fill.field, BsonValue::Double(value));
                        }
                    }
                }
            }
        }
    }

    // `locf` and `linear` emit documents in the sort order they used; `value`
    // fills keep the caller's order.
    if ordered {
        let mut ordered_documents = Vec::with_capacity(working.len());
        for (_, members) in &groups {
            for position in members {
                ordered_documents.push(working[*position].clone());
            }
        }
        return Ok(ordered_documents);
    }
    Ok(working)
}

/// `$redact` sentinels. They must not collide with ordinary string values
/// because `"$$KEEP"` produced as a literal is an error in `mongod`.
pub(crate) const REDACT_KEEP: &str = "\u{1}$KEEP";
pub(crate) const REDACT_DESCEND: &str = "\u{1}$DESCEND";
pub(crate) const REDACT_PRUNE: &str = "\u{1}$PRUNE";

pub(crate) fn validate_redact_variables(expression: &BsonValue, variables: &ExpressionVariables) -> Result<(), CommandError> {
    const RESERVED: [&str; 5] = ["ROOT", "CURRENT", "KEEP", "DESCEND", "PRUNE"];
    match expression {
        BsonValue::String(name) if name.starts_with("$$") => {
            let variable = &name[2..];
            if !RESERVED.contains(&variable) && !variables.contains_key(variable) {
                return Err(CommandError::new(17276, format!("Use of undefined variable: {variable}")));
            }
        }
        BsonValue::Document(document) => {
            for (_, value) in &document.fields {
                validate_redact_variables(value, variables)?;
            }
        }
        BsonValue::Array(items) => {
            for item in items {
                validate_redact_variables(item, variables)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn redact_value(
    value: &BsonValue,
    expression: &BsonValue,
    variables: &ExpressionVariables,
    root: &BsonDocument,
) -> Result<Option<BsonValue>, CommandError> {
    match value {
        BsonValue::Document(document) => Ok(redact_document(document, expression, variables, root)?.map(BsonValue::Document)),
        BsonValue::Array(items) => {
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                if let Some(value) = redact_value(item, expression, variables, root)? {
                    kept.push(value);
                }
            }
            Ok(Some(BsonValue::Array(kept)))
        }
        other => Ok(Some(other.clone())),
    }
}

pub(crate) fn redact_document(
    document: &BsonDocument,
    expression: &BsonValue,
    variables: &ExpressionVariables,
    root: &BsonDocument,
) -> Result<Option<BsonDocument>, CommandError> {
    let mut bound = variables.clone();
    bound.insert("ROOT".to_string(), BsonValue::Document(root.clone()));
    bound.insert("CURRENT".to_string(), BsonValue::Document(document.clone()));
    bound.insert("KEEP".to_string(), BsonValue::String(REDACT_KEEP.to_string()));
    bound.insert("DESCEND".to_string(), BsonValue::String(REDACT_DESCEND.to_string()));
    bound.insert("PRUNE".to_string(), BsonValue::String(REDACT_PRUNE.to_string()));
    let decision = evaluate_expression_optional_vars(document, expression, &bound)?;
    match decision {
        Some(BsonValue::String(name)) if name == REDACT_KEEP => Ok(Some(document.clone())),
        Some(BsonValue::String(name)) if name == REDACT_PRUNE => Ok(None),
        Some(BsonValue::String(name)) if name == REDACT_DESCEND => {
            let mut descended = BsonDocument::new();
            for (field, value) in &document.fields {
                if let Some(value) = redact_value(value, expression, variables, root)? {
                    descended.push(field.clone(), value);
                }
            }
            Ok(Some(descended))
        }
        other => {
            let rendered = match &other {
                Some(value) => render_shell_value(value),
                None => "MISSING".to_string(),
            };
            Err(CommandError::new(
                17053,
                format!(
                    "PlanExecutor error during aggregation :: caused by :: $redact's expression should not return anything aside from the variables $$KEEP, $$DESCEND, and $$PRUNE, but returned {rendered}"
                ),
            ))
        }
    }
}

pub(crate) fn redact_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    validate_redact_variables(body, variables)?;
    let mut output = Vec::with_capacity(documents.len());
    for document in &documents {
        if let Some(redacted) = redact_document(document, body, variables, document)? {
            output.push(redacted);
        }
    }
    Ok(output)
}

pub(crate) fn out_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
) -> Result<Vec<BsonDocument>, CommandError> {
    let target = stage_target_namespace(body, namespace, "$out")?;
    // `$out` is atomic in `mongod`: it writes every document first and only
    // replaces the target once the whole output set satisfies the target's
    // unique indexes. Validate that here so a failure cannot leave the old
    // collection half-deleted.
    let mut staged_documents = Vec::with_capacity(documents.len());
    for mut document in documents {
        if document.get("_id").is_none() {
            document.push("_id".to_string(), BsonValue::ObjectId(generate_object_id()));
        }
        staged_documents.push(document);
    }
    let temporary = temporary_aggregation_namespace(&target);
    let mut identifiers: BTreeSet<Vec<u8>> = BTreeSet::new();
    for document in &staged_documents {
        let identifier = document.get("_id").cloned().unwrap_or(BsonValue::Null);
        if !identifiers.insert(value_to_key(&identifier)) {
            let mut key_value = BsonDocument::new();
            key_value.push("_id".to_string(), identifier);
            return Err(out_duplicate_error(&temporary, "_id_", &key_value));
        }
    }
    let indexes = store.index_specs(&target)?;
    for specification in indexes.iter().filter(|index| index.unique) {
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        for document in &staged_documents {
            for tuple in index_entries(document, specification) {
                if !seen.insert(index_tuple(&tuple)) {
                    let mut key_value = BsonDocument::new();
                    for ((field, _), value) in specification.key.fields.iter().zip(&tuple) {
                        key_value.push(field.clone(), value.clone());
                    }
                    return Err(out_duplicate_error(&temporary, &specification.name, &key_value));
                }
            }
        }
    }
    let target_keys: Vec<Vec<u8>> = namespace_documents(store, &target, session)?.into_iter().map(|(key, _)| key).collect();
    if !target_keys.is_empty() {
        stage_delete(store, session, &target, &target_keys)?;
    }
    let entries: Vec<(BsonDocument, Vec<u8>)> = staged_documents
        .into_iter()
        .map(|document| {
            let identifier = document.get("_id").cloned().unwrap_or(BsonValue::Null);
            let key = value_to_key(&identifier);
            (document, key)
        })
        .collect();
    stage_insert(store, session, &target, &entries)?;
    Ok(Vec::new())
}

/// One `$merge` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeMode {
    Replace,
    Merge,
    KeepExisting,
    Fail,
    Pipeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeNotMatched {
    Insert,
    Discard,
    Fail,
}

pub(crate) fn merge_mode(value: Option<&BsonValue>) -> Result<MergeMode, CommandError> {
    match value {
        None => Ok(MergeMode::Merge),
        Some(BsonValue::String(name)) => match name.as_str() {
            "replace" => Ok(MergeMode::Replace),
            "merge" => Ok(MergeMode::Merge),
            "keepExisting" => Ok(MergeMode::KeepExisting),
            "fail" => Ok(MergeMode::Fail),
            other => Err(CommandError::new(2, format!("Enumeration value '{other}' for field 'whenMatched' is not a valid value."))),
        },
        Some(BsonValue::Array(stages)) if stages.is_empty() => Ok(MergeMode::Pipeline),
        Some(BsonValue::Array(_)) => Ok(MergeMode::Pipeline),
        Some(other) => {
            Err(CommandError::new(2, format!("Enumeration value '{}' for field 'whenMatched' is not a valid value.", bson_type_name(other))))
        }
    }
}

pub(crate) fn merge_not_matched(value: Option<&BsonValue>) -> Result<MergeNotMatched, CommandError> {
    match value {
        None => Ok(MergeNotMatched::Insert),
        Some(BsonValue::String(name)) => match name.as_str() {
            "insert" => Ok(MergeNotMatched::Insert),
            "discard" => Ok(MergeNotMatched::Discard),
            "fail" => Ok(MergeNotMatched::Fail),
            other => Err(CommandError::new(2, format!("Enumeration value '{other}' for field '$merge.whenNotMatched' is not a valid value."))),
        },
        Some(other) => Err(CommandError::new(
            2,
            format!("Enumeration value '{}' for field '$merge.whenNotMatched' is not a valid value.", bson_type_name(other)),
        )),
    }
}

/// Applies a `whenMatched` update pipeline to an existing target document.
pub(crate) fn merge_update_pipeline(
    mut target: BsonDocument,
    stages: &[BsonValue],
    variables: &ExpressionVariables,
) -> Result<BsonDocument, CommandError> {
    let original_id = target.get("_id").cloned();
    for stage in stages {
        let BsonValue::Document(stage_document) = stage
        else {
            return Err(CommandError::new(2, "aggregate stage must be a document"));
        };
        let Some((name, body)) = stage_document.fields.first()
        else {
            return Err(CommandError::new(2, "aggregate stage is empty"));
        };
        if stage_document.fields.len() != 1 {
            return Err(CommandError::new(2, "aggregate stage must contain exactly one field"));
        }
        match name.as_str() {
            "$set" | "$addFields" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, format!("{name} requires a document")));
                };
                for (field, expression) in &specification.fields {
                    let value = evaluate_expression_vars(&target, expression, variables)?;
                    let segments: Vec<&str> = field.split('.').collect();
                    set_aggregation_segments(&mut target, &segments, value)?;
                }
            }
            "$unset" => {
                let fields = match body {
                    BsonValue::String(field) => vec![field.clone()],
                    BsonValue::Array(items) => items
                        .iter()
                        .filter_map(|item| match item {
                            BsonValue::String(field) => Some(field.clone()),
                            _ => None,
                        })
                        .collect(),
                    _ => return Err(CommandError::new(2, "$unset requires a string or an array of strings")),
                };
                for field in fields {
                    remove_field_path(&mut target, &field);
                }
            }
            "$replaceWith" => match evaluate_expression_optional_vars(&target, body, variables)? {
                Some(BsonValue::Document(replacement)) => target = replacement,
                Some(other) => {
                    return Err(CommandError::new(
                        40228,
                        format!(
                            "PlanExecutor error during aggregation :: caused by :: 'replacement document'  must evaluate to an object, but resulting value was: {}. Type of resulting value: '{}'. Input document: {{}}",
                            render_shell_value(&other),
                            bson_type_name(&other)
                        ),
                    ));
                }
                None => {
                    return Err(CommandError::new(
                        40228,
                        "PlanExecutor error during aggregation :: caused by :: 'replacement document'  must evaluate to an object, but resulting value was: MISSING. Type of resulting value: 'missing'. Input document: {}",
                    ));
                }
            },
            "$project" => {
                let BsonValue::Document(specification) = body
                else {
                    return Err(CommandError::new(2, "$project requires a document"));
                };
                target = project_stage(std::slice::from_ref(&target), specification)?.remove(0);
            }
            "$match" => {
                return Err(CommandError::new(
                    72,
                    "PlanExecutor error during aggregation :: caused by :: $match is not allowed to be used within an update",
                ));
            }
            other => {
                return Err(CommandError::new(
                    59,
                    format!("unsupported update pipeline stage: {other} (supported: $set, $addFields, $unset, $replaceWith, $project)"),
                ));
            }
        }
    }
    if let Some(identifier) = original_id {
        match target.get("_id") {
            Some(updated) if value_to_key(updated) == value_to_key(&identifier) => {}
            Some(updated) => {
                return Err(merge_immutable_id_error(updated));
            }
            None => target.push("_id".to_string(), identifier),
        }
    }
    Ok(target)
}

pub(crate) fn merge_immutable_id_error(identifier: &BsonValue) -> CommandError {
    CommandError::new(
        66,
        format!(
            "PlanExecutor error during aggregation :: caused by :: $merge failed to update the matching document, did you attempt to modify the _id or the shard key? :: caused by :: After applying the update, the (immutable) field '_id' was found to have been altered to _id: {}",
            render_shell_value(identifier)
        ),
    )
}

pub(crate) fn merge_immutable_field_error() -> CommandError {
    CommandError::new(
        66,
        "PlanExecutor error during aggregation :: caused by :: $merge failed to update the matching document, did you attempt to modify the _id or the shard key? :: caused by :: Performing an update on the path '_id' would modify the immutable field '_id'",
    )
}

pub(crate) fn merge_duplicate_error(
    namespace: &str,
    index_name: &str,
    key_pattern: &BsonDocument,
    key_value: &BsonDocument,
    when_matched_fail: bool,
) -> CommandError {
    let mut key_text = String::new();
    for (field, value) in &key_value.fields {
        if !key_text.is_empty() {
            key_text.push_str(", ");
        }
        key_text.push_str(field);
        key_text.push_str(": ");
        key_text.push_str(&render_shell_value(value));
    }
    let prefix = if when_matched_fail {
        "PlanExecutor error during aggregation :: caused by :: $merge with whenMatched: fail found an existing document with the same values for the 'on' fields :: caused by :: "
    }
    else {
        "PlanExecutor error during aggregation :: caused by :: $merge failed due to a DuplicateKey error :: caused by :: "
    };
    CommandError::new(
        11000,
        format!("{prefix}E11000 duplicate key error collection: {namespace} index: {index_name} dup key: {{ {key_text} }}"),
    )
    .with_detail("keyPattern", BsonValue::Document(key_pattern.clone()))
    .with_detail("keyValue", BsonValue::Document(key_value.clone()))
}

pub(crate) fn merge_key_value(document: &BsonDocument, key_pattern: &BsonDocument) -> BsonDocument {
    let mut key_value = BsonDocument::new();
    for (field, _) in &key_pattern.fields {
        key_value.push(field.clone(), lookup_field(document, field).cloned().unwrap_or(BsonValue::Null));
    }
    key_value
}

/// `$merge`: upserts the pipeline output into `into` using `on` as the join
/// key, with the documented `whenMatched` / `whenNotMatched` modes.
pub(crate) fn merge_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(2, "$merge requires a document"));
    };
    let target = merge_target_namespace(specification, namespace)?;
    let on_fields = merge_on_fields(specification)?;
    let mode = merge_mode(specification.get("whenMatched"))?;
    let not_matched = merge_not_matched(specification.get("whenNotMatched"))?;
    if matches!(mode, MergeMode::KeepExisting | MergeMode::Fail) && not_matched != MergeNotMatched::Insert {
        let matched = if matches!(mode, MergeMode::KeepExisting) { "keepExisting" } else { "fail" };
        let missing = match not_matched {
            MergeNotMatched::Insert => "insert",
            MergeNotMatched::Discard => "discard",
            MergeNotMatched::Fail => "fail",
        };
        return Err(CommandError::new(
            51181,
            format!("Combination of $merge modes 'whenMatched: {matched}' and 'whenNotMatched: {missing}' is not supported"),
        ));
    }
    let when_matched_pipeline = match specification.get("whenMatched") {
        Some(BsonValue::Array(stages)) => stages.clone(),
        _ => Vec::new(),
    };
    let let_variables = match specification.get("let") {
        Some(BsonValue::Document(bindings)) => bindings.clone(),
        _ => BsonDocument::new(),
    };

    // A missing target is created before the index check, exactly like `mongod`.
    if !store.namespace_exists(&target)? {
        store.import(&target, &[]).map_err(|error| CommandError::new(2, format!("$merge failed: {error}")))?;
    }
    let Some((index_name, key_pattern)) = merge_unique_index(store, &target, &on_fields)?
    else {
        return Err(CommandError::new(51183, "Cannot find index to verify that join fields will be unique"));
    };

    let mut target_documents: Vec<BsonDocument> = collect_matching(store, &target, &BsonDocument::new(), session)?;

    // `mongod` validates every document's `on` values before applying any
    // write, so a later document can reject the whole aggregation.
    let mut on_values_by_document: Vec<Vec<BsonValue>> = Vec::with_capacity(documents.len());
    for document in &documents {
        let mut on_values = Vec::new();
        for field in &on_fields {
            let value = lookup_field(document, field).cloned().unwrap_or(BsonValue::Null);
            if matches!(value, BsonValue::Null | BsonValue::Undefined | BsonValue::Array(_)) {
                return Err(CommandError::new(
                    51132,
                    format!(
                        "PlanExecutor error during aggregation :: caused by :: $merge write error: 'on' field '{field}' cannot be missing, null, undefined or an array"
                    ),
                ));
            }
            on_values.push(value);
        }
        on_values_by_document.push(on_values);
    }

    for (document, on_values) in documents.into_iter().zip(on_values_by_document) {
        let matched_position = target_documents.iter().position(|candidate| {
            on_fields.iter().zip(&on_values).all(|(field, value)| {
                lookup_field(candidate, field).map(|candidate_value| compare_values(candidate_value, value) == Ordering::Equal).unwrap_or(false)
            })
        });

        if let Some(position) = matched_position {
            let existing = target_documents[position].clone();
            let mut bound = variables.clone();
            for (name, expression) in &let_variables.fields {
                bound.insert(name.clone(), evaluate_expression_vars(&existing, expression, variables)?);
            }
            let updated = match mode {
                MergeMode::KeepExisting => continue,
                MergeMode::Fail => {
                    let same_identifier = match (existing.get("_id"), document.get("_id")) {
                        (Some(left), Some(right)) => value_to_key(left) == value_to_key(right),
                        _ => false,
                    };
                    if index_name == "_id_" || same_identifier {
                        let mut id_pattern = BsonDocument::new();
                        id_pattern.push("_id".to_string(), BsonValue::Int32(1));
                        let id_value = merge_key_value(&existing, &id_pattern);
                        return Err(merge_duplicate_error(&target, "_id_", &id_pattern, &id_value, index_name == "_id_"));
                    }
                    let on_value = merge_key_value(&existing, &key_pattern);
                    return Err(merge_duplicate_error(&target, &index_name, &key_pattern, &on_value, true));
                }
                MergeMode::Replace => {
                    let mut replacement = document.clone();
                    if let Some(identifier) = existing.get("_id") {
                        match document.get("_id") {
                            Some(new_identifier) if value_to_key(new_identifier) != value_to_key(identifier) => {
                                return Err(merge_immutable_id_error(new_identifier));
                            }
                            Some(_) => {}
                            None => replacement.push("_id".to_string(), identifier.clone()),
                        }
                    }
                    replacement
                }
                MergeMode::Merge => merge_document(&existing, &document)?,
                MergeMode::Pipeline => merge_update_pipeline(existing, &when_matched_pipeline, &bound)?,
            };
            target_documents[position] = updated.clone();
            let identifier = updated.get("_id").cloned().unwrap_or(BsonValue::Null);
            // `$merge` applies each document as it goes: a later failure does not
            // roll back the writes already made, which is what `mongod` does.
            stage_replace(store, session, &target, &[(value_to_key(&identifier), encode_bson(&updated))])?;
        }
        else {
            match not_matched {
                MergeNotMatched::Discard => continue,
                MergeNotMatched::Fail => {
                    return Err(CommandError::new(
                        13113,
                        "PlanExecutor error during aggregation :: caused by :: $merge could not find a matching document in the target collection for at least one document in the source collection",
                    ));
                }
                MergeNotMatched::Insert => {}
            }
            let mut inserted = document.clone();
            let identifier = match inserted.get("_id").cloned() {
                Some(identifier) => identifier,
                None => {
                    let identifier = BsonValue::ObjectId(generate_object_id());
                    inserted.push("_id".to_string(), identifier.clone());
                    identifier
                }
            };
            let identifier_key = value_to_key(&identifier);
            if let Some((conflict_name, conflict_pattern, conflict_value)) = merge_conflict(&target_documents, &inserted, &identifier_key) {
                return Err(merge_duplicate_error(&target, &conflict_name, &conflict_pattern, &conflict_value, false));
            }
            target_documents.push(inserted.clone());
            stage_insert(store, session, &target, &[(inserted, identifier_key)])?;
        }
    }

    Ok(Vec::new())
}

pub(crate) fn merge_target_namespace(specification: &BsonDocument, namespace: &str) -> Result<String, CommandError> {
    match specification.get("into") {
        Some(BsonValue::String(collection)) if !collection.is_empty() => {
            let database = namespace.split_once('.').map(|(database, _)| database).unwrap_or("admin");
            Ok(format!("{database}.{collection}"))
        }
        Some(BsonValue::Document(specification)) => {
            let database = match specification.get("db") {
                Some(BsonValue::String(name)) if !name.is_empty() => name.clone(),
                _ => {
                    return Err(CommandError::new(51178, "BSON field '$merge.into.db' is missing but a required field"));
                }
            };
            let collection = match specification.get("coll") {
                Some(BsonValue::String(name)) if !name.is_empty() => name.clone(),
                _ => {
                    return Err(CommandError::new(51178, "BSON field '$merge.into.coll' is missing but a required field"));
                }
            };
            Ok(format!("{database}.{collection}"))
        }
        Some(other) => Err(CommandError::new(
            51178,
            format!("$merge 'into' field  must be either a string or an object, but found {}", bson_type_name(other)),
        )),
        None => Err(CommandError::new(51178, "$merge requires an 'into' field")),
    }
}

pub(crate) fn merge_on_fields(specification: &BsonDocument) -> Result<Vec<String>, CommandError> {
    match specification.get("on") {
        None => Ok(vec!["_id".to_string()]),
        Some(BsonValue::String(field)) => Ok(vec![field.clone()]),
        Some(BsonValue::Array(items)) => {
            if items.is_empty() {
                return Err(CommandError::new(51187, "If explicitly specifying $merge 'on', must include at least one field"));
            }
            let mut fields = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    BsonValue::String(field) => fields.push(field.clone()),
                    other => {
                        return Err(CommandError::new(
                            51186,
                            format!("$merge 'on' field  must be either a string or an array of strings, but found {}", bson_type_name(other)),
                        ));
                    }
                }
            }
            Ok(fields)
        }
        Some(other) => Err(CommandError::new(
            51186,
            format!("$merge 'on' field  must be either a string or an array of strings, but found {}", bson_type_name(other)),
        )),
    }
}

/// The unique index backing the `on` fields, as `(name, key pattern)`.
pub(crate) fn merge_unique_index(
    store: &dyn MongoStorage,
    namespace: &str,
    on_fields: &[String],
) -> Result<Option<(String, BsonDocument)>, CommandError> {
    if on_fields.len() == 1 && on_fields[0] == "_id" {
        let mut pattern = BsonDocument::new();
        pattern.push("_id".to_string(), BsonValue::Int32(1));
        return Ok(Some(("_id_".to_string(), pattern)));
    }
    for spec in store.index_specs(namespace)? {
        if !spec.unique || spec.key.fields.len() != on_fields.len() {
            continue;
        }
        if spec.key.fields.iter().zip(on_fields).all(|((field, _), wanted)| field == wanted) {
            return Ok(Some((spec.name.clone(), spec.key.clone())));
        }
    }
    Ok(None)
}

pub(crate) fn merge_document(existing: &BsonDocument, incoming: &BsonDocument) -> Result<BsonDocument, CommandError> {
    if let (Some(left), Some(right)) = (existing.get("_id"), incoming.get("_id"))
        && value_to_key(left) != value_to_key(right)
    {
        return Err(merge_immutable_field_error());
    }
    let mut merged = existing.clone();
    for (field, value) in &incoming.fields {
        if let Some(entry) = merged.fields.iter_mut().find(|(key, _)| key == field) {
            entry.1 = value.clone();
        }
        else {
            merged.push(field.clone(), value.clone());
        }
    }
    Ok(merged)
}

/// First duplicate-key conflict an insert would create, as
/// `(index name, key pattern, key value)`.
pub(crate) fn merge_conflict(
    documents: &[BsonDocument],
    inserted: &BsonDocument,
    identifier: &[u8],
) -> Option<(String, BsonDocument, BsonDocument)> {
    for candidate in documents {
        let candidate_id = candidate.get("_id").map(value_to_key).unwrap_or_default();
        if candidate_id == identifier {
            let mut pattern = BsonDocument::new();
            pattern.push("_id".to_string(), BsonValue::Int32(1));
            let value = merge_key_value(inserted, &pattern);
            return Some(("_id_".to_string(), pattern, value));
        }
    }
    None
}

/// `$facet`: every branch consumes the same input and the stage emits a single
/// document holding one array per branch, in specification order.
pub(crate) fn facet_stage(
    documents: Vec<BsonDocument>,
    body: &BsonValue,
    store: &dyn MongoStorage,
    namespace: &str,
    session: Option<&str>,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(2, "$facet requires a document"));
    };
    let mut faceted = BsonDocument::new();
    for (name, branch) in &specification.fields {
        let BsonValue::Array(stages) = branch
        else {
            return Err(CommandError::new(2, format!("$facet branch `{name}` must be a pipeline array")));
        };
        reject_stage_outside(stages, 40600, "is not allowed to be used within a $facet stage")?;
        let branch_output = run_pipeline(documents.clone(), stages, store, namespace, session, variables)?;
        faceted.push(name.clone(), BsonValue::Array(branch_output.into_iter().map(BsonValue::Document).collect()));
    }
    Ok(vec![faceted])
}

/// `$bucket`: fixed boundaries, an optional `default` bucket, and accumulator
/// output. Empty buckets are omitted and the default bucket is emitted last.
pub(crate) fn bucket_stage(
    documents: &[BsonDocument],
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(2, "$bucket requires a document"));
    };
    let Some(group_by) = specification.get("groupBy")
    else {
        return Err(CommandError::new(2, "$bucket requires a `groupBy` expression"));
    };
    let Some(BsonValue::Array(boundaries)) = specification.get("boundaries")
    else {
        return Err(CommandError::new(2, "$bucket requires a `boundaries` array"));
    };
    if boundaries.len() < 2 {
        return Err(CommandError::new(2, "$bucket requires at least two boundaries"));
    }
    for pair in boundaries.windows(2) {
        if compare_values(&pair[0], &pair[1]) != Ordering::Less {
            return Err(CommandError::new(
                2,
                format!(
                    "$bucket boundaries must be in ascending order, found {} before {}",
                    render_shell_value(&pair[0]),
                    render_shell_value(&pair[1])
                ),
            ));
        }
    }
    let default = specification.get("default").cloned();
    let accumulators = bucket_accumulators(specification)?;

    let mut members: Vec<Vec<usize>> = vec![Vec::new(); boundaries.len() - 1];
    let mut default_members: Vec<usize> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let value = evaluate_expression_vars(document, group_by, variables)?;
        let mut placed = false;
        for bucket in 0..boundaries.len() - 1 {
            let above_lower = compare_values(&value, &boundaries[bucket]) != Ordering::Less;
            let below_upper = compare_values(&value, &boundaries[bucket + 1]) == Ordering::Less;
            if above_lower && below_upper {
                members[bucket].push(index);
                placed = true;
                break;
            }
        }
        if !placed {
            let Some(default) = &default
            else {
                return Err(CommandError::new(
                    7158303,
                    "PlanExecutor error during aggregation :: caused by :: $switch could not find a matching branch for an input, and no default was specified.",
                ));
            };
            let _ = default;
            default_members.push(index);
        }
    }

    let mut output = Vec::new();
    for (bucket, group) in members.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let mut bucketed = BsonDocument::new();
        bucketed.push("_id".to_string(), boundaries[bucket].clone());
        for accumulator in &accumulators {
            bucketed.push(accumulator.field.clone(), accumulate(accumulator, documents, group)?);
        }
        output.push(bucketed);
    }
    if !default_members.is_empty() {
        let mut bucketed = BsonDocument::new();
        bucketed.push("_id".to_string(), default.unwrap_or(BsonValue::Null));
        for accumulator in &accumulators {
            bucketed.push(accumulator.field.clone(), accumulate(accumulator, documents, &default_members)?);
        }
        output.push(bucketed);
    }
    Ok(output)
}

pub(crate) fn bucket_accumulators(specification: &BsonDocument) -> Result<Vec<Accumulator>, CommandError> {
    match specification.get("output") {
        Some(BsonValue::Document(output)) => {
            let mut accumulators = Vec::new();
            for (field, value) in &output.fields {
                accumulators.push(parse_accumulator(field, value)?);
            }
            Ok(accumulators)
        }
        Some(_) => Err(CommandError::new(2, "$bucket `output` must be a document")),
        None => Ok(vec![Accumulator { field: "count".to_string(), kind: AccumulatorKind::Sum, argument: BsonValue::Int32(1) }]),
    }
}

pub(crate) fn parse_accumulator(field: &str, value: &BsonValue) -> Result<Accumulator, CommandError> {
    let BsonValue::Document(operator_document) = value
    else {
        return Err(CommandError::new(2, format!("accumulator `{field}` must be a document")));
    };
    if operator_document.fields.len() != 1 {
        return Err(CommandError::new(2, format!("accumulator `{field}` must contain exactly one operator")));
    }
    let (operator, argument) = &operator_document.fields[0];
    let Some(kind) = AccumulatorKind::parse(operator)
    else {
        return Err(CommandError::new(
            59,
            format!("unknown accumulator: {operator} (supported: $sum, $avg, $min, $max, $push, $addToSet, $first, $last)"),
        ));
    };
    Ok(Accumulator { field: field.to_string(), kind, argument: argument.clone() })
}

/// `$bucketAuto`: at most `buckets` buckets over sorted `groupBy` values.
///
/// Equal values always land in the same bucket, which is why the bucket count
/// can be lower than the request when the input has few distinct values.
pub(crate) fn bucket_auto_stage(
    documents: &[BsonDocument],
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(2, "$bucketAuto requires a document"));
    };
    let Some(group_by) = specification.get("groupBy")
    else {
        return Err(CommandError::new(2, "$bucketAuto requires a `groupBy` expression"));
    };
    let buckets = match specification.get("buckets") {
        Some(value) => match int_value(value) {
            Some(count) if count > 0 => count as usize,
            _ => return Err(CommandError::new(2, "$bucketAuto requires a positive `buckets` count")),
        },
        None => return Err(CommandError::new(2, "$bucketAuto requires `buckets`")),
    };
    let output = match specification.get("output") {
        Some(BsonValue::Document(output)) => {
            let mut accumulators = Vec::new();
            for (field, value) in &output.fields {
                accumulators.push(parse_accumulator(field, value)?);
            }
            Some(accumulators)
        }
        Some(_) => return Err(CommandError::new(2, "$bucketAuto `output` must be a document")),
        None => None,
    };

    let mut keys: Vec<(BsonValue, usize)> = Vec::with_capacity(documents.len());
    for (index, document) in documents.iter().enumerate() {
        keys.push((evaluate_expression_vars(document, group_by, variables)?, index));
    }
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    // Stable sort keeps equal values in input order.
    keys.sort_by(|left, right| compare_values(&left.0, &right.0));

    let target = keys.len().div_ceil(buckets);
    let mut output_documents = Vec::new();
    let mut start = 0_usize;
    while start < keys.len() {
        let mut end = start;
        let mut taken = 0_usize;
        while end < keys.len() {
            let group_start = end;
            let value = keys[end].0.clone();
            while end < keys.len() && compare_values(&keys[end].0, &value) == Ordering::Equal {
                end += 1;
            }
            taken += end - group_start;
            if taken >= target {
                break;
            }
        }
        let min = keys[start].0.clone();
        let max = if end < keys.len() { keys[end].0.clone() } else { keys[end - 1].0.clone() };
        let members: Vec<usize> = keys[start..end].iter().map(|(_, index)| *index).collect();
        let mut identifier = BsonDocument::new();
        identifier.push("min".to_string(), min);
        identifier.push("max".to_string(), max);
        let mut bucketed = BsonDocument::new();
        bucketed.push("_id".to_string(), BsonValue::Document(identifier));
        match &output {
            Some(accumulators) => {
                for accumulator in accumulators {
                    bucketed.push(accumulator.field.clone(), accumulate(accumulator, documents, &members)?);
                }
            }
            None => bucketed.push("count".to_string(), count_value(members.len() as i64)),
        }
        output_documents.push(bucketed);
        start = end;
    }
    Ok(output_documents)
}

/// One window output field: `$documentNumber`, `$rank`, `$denseRank` or `$sum`.
pub(crate) struct WindowOutput {
    pub(crate) field: String,
    pub(crate) kind: WindowKind,
    pub(crate) argument: BsonValue,
    pub(crate) window: Option<(WindowBound, WindowBound)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowKind {
    DocumentNumber,
    Rank,
    DenseRank,
    Sum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowBound {
    Unbounded,
    Current,
    Offset(i64),
}

/// `$setWindowFields`: partitions, sorts, and writes the supported window
/// fields. Partitions are emitted in ascending partition-key order and each
/// partition in `sortBy` order, matching `mongod 8.0.9`.
pub(crate) fn set_window_fields_stage(
    documents: &[BsonDocument],
    body: &BsonValue,
    variables: &ExpressionVariables,
) -> Result<Vec<BsonDocument>, CommandError> {
    let BsonValue::Document(specification) = body
    else {
        return Err(CommandError::new(2, "$setWindowFields requires a document"));
    };
    let partition_by = specification.get("partitionBy");
    let sort = match specification.get("sortBy") {
        Some(BsonValue::Document(sort)) => parse_sort_stage(sort)?,
        Some(_) => return Err(CommandError::new(2, "$setWindowFields `sortBy` must be a document")),
        None => Vec::new(),
    };
    let Some(BsonValue::Document(output_specification)) = specification.get("output")
    else {
        return Err(CommandError::new(2, "$setWindowFields requires an `output` document"));
    };

    let mut outputs: Vec<WindowOutput> = Vec::new();
    for (field, value) in &output_specification.fields {
        let BsonValue::Document(operator_document) = value
        else {
            return Err(CommandError::new(2, format!("$setWindowFields field `{field}` must be a document")));
        };
        let mut kind: Option<WindowKind> = None;
        let mut argument = BsonValue::Null;
        let mut window: Option<(WindowBound, WindowBound)> = None;
        for (operator, operand) in &operator_document.fields {
            match operator.as_str() {
                "$documentNumber" => kind = Some(WindowKind::DocumentNumber),
                "$rank" => kind = Some(WindowKind::Rank),
                "$denseRank" => kind = Some(WindowKind::DenseRank),
                "$sum" => {
                    kind = Some(WindowKind::Sum);
                    argument = operand.clone();
                }
                "window" => window = Some(parse_window(operand)?),
                other => {
                    return Err(CommandError::new(
                        59,
                        format!("unknown window operator: {other} (supported: $documentNumber, $rank, $denseRank, $sum)"),
                    ));
                }
            }
        }
        let Some(kind) = kind
        else {
            return Err(CommandError::new(2, format!("$setWindowFields field `{field}` has no operator")));
        };
        let required_sort = match kind {
            WindowKind::DocumentNumber => "$documentNumber",
            WindowKind::Rank => "$rank",
            WindowKind::DenseRank => "$denseRank",
            WindowKind::Sum => "",
        };
        if !required_sort.is_empty() {
            if sort.len() != 1 {
                return Err(CommandError::new(
                    5371602,
                    format!("{required_sort} must be specified with a top level sortBy expression with exactly one element"),
                ));
            }
            if window.is_some() {
                return Err(CommandError::new(2, format!("{required_sort} does not accept a `window`")));
            }
        }
        outputs.push(WindowOutput { field: field.clone(), kind, argument, window });
    }

    let mut partitions: Vec<(BsonValue, Vec<usize>)> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let key = match partition_by {
            Some(expression) => evaluate_expression_vars(document, expression, variables)?,
            None => BsonValue::Null,
        };
        match partitions.iter_mut().find(|(existing, _)| values_equal(Some(existing), &key)) {
            Some((_, members)) => members.push(index),
            None => partitions.push((key, vec![index])),
        }
    }
    partitions.sort_by(|left, right| compare_values(&left.0, &right.0));
    for (_, members) in &mut partitions {
        members.sort_by(|left, right| compare_documents(documents, *left, *right, &sort));
    }

    let mut result = Vec::new();
    for (_, members) in &partitions {
        let mut dense_rank = 0_i32;
        let mut previous_key: Option<Vec<BsonValue>> = None;
        for (position, index) in members.iter().enumerate() {
            let key = sort_keys(&documents[*index], &sort);
            let same_as_previous = previous_key.as_ref().map(|previous| previous == &key).unwrap_or(false);
            if position == 0 || !same_as_previous {
                dense_rank += 1;
            }
            let rank = if position == 0 {
                1
            }
            else if same_as_previous {
                // The rank of a tie is the position of the first document in its group.
                let mut first = position;
                while first > 0 && sort_keys(&documents[members[first - 1]], &sort) == key {
                    first -= 1;
                }
                (first + 1) as i32
            }
            else {
                (position + 1) as i32
            };
            previous_key = Some(key);

            let mut expanded = documents[*index].clone();
            for output in &outputs {
                let value = match output.kind {
                    WindowKind::DocumentNumber => count_value((position + 1) as i64),
                    WindowKind::Rank => count_value(i64::from(rank)),
                    WindowKind::DenseRank => count_value(i64::from(dense_rank)),
                    WindowKind::Sum => {
                        let (start, end) = output.window.unwrap_or((WindowBound::Unbounded, WindowBound::Unbounded));
                        let window_members = window_range(members, position, start, end);
                        let accumulator =
                            Accumulator { field: output.field.clone(), kind: AccumulatorKind::Sum, argument: output.argument.clone() };
                        accumulate(&accumulator, documents, &window_members)?
                    }
                };
                set_field_path(&mut expanded, &output.field, value);
            }
            result.push(expanded);
        }
    }
    Ok(result)
}

pub(crate) fn parse_window(value: &BsonValue) -> Result<(WindowBound, WindowBound), CommandError> {
    let BsonValue::Document(specification) = value
    else {
        return Err(CommandError::new(2, "`window` must be a document"));
    };
    if specification.get("range").is_some() {
        return Err(CommandError::new(59, "`window.range` is outside this gateway (supported: `window.documents`)"));
    }
    let Some(BsonValue::Array(bounds)) = specification.get("documents")
    else {
        return Err(CommandError::new(2, "`window` requires a `documents` array"));
    };
    if bounds.len() != 2 {
        return Err(CommandError::new(2, "`window.documents` requires exactly two bounds"));
    }
    Ok((parse_window_bound(&bounds[0])?, parse_window_bound(&bounds[1])?))
}

pub(crate) fn parse_window_bound(value: &BsonValue) -> Result<WindowBound, CommandError> {
    match value {
        BsonValue::String(text) if text == "unbounded" => Ok(WindowBound::Unbounded),
        BsonValue::String(text) if text == "current" => Ok(WindowBound::Current),
        _ => match int_value(value) {
            Some(offset) => Ok(WindowBound::Offset(offset)),
            None => Err(CommandError::new(2, "`window.documents` bounds must be `unbounded`, `current` or an integer")),
        },
    }
}

pub(crate) fn window_range(members: &[usize], position: usize, start: WindowBound, end: WindowBound) -> Vec<usize> {
    let last = members.len().saturating_sub(1);
    let start_index = match start {
        WindowBound::Unbounded => 0,
        WindowBound::Current => position,
        WindowBound::Offset(offset) => offset_position(position, offset, last),
    };
    let end_index = match end {
        WindowBound::Unbounded => last,
        WindowBound::Current => position,
        WindowBound::Offset(offset) => offset_position(position, offset, last),
    };
    if start_index > end_index || start_index > last {
        return Vec::new();
    }
    members[start_index..=end_index.min(last)].to_vec()
}

pub(crate) fn offset_position(position: usize, offset: i64, last: usize) -> usize {
    if offset >= 0 { position.saturating_add(offset as usize).min(last) } else { position.saturating_sub((-offset) as usize) }
}

pub(crate) fn sort_keys(document: &BsonDocument, sort: &[(String, i32)]) -> Vec<BsonValue> {
    sort.iter().map(|(field, direction)| sort_key(document, field, *direction)).collect()
}

pub(crate) fn compare_documents(documents: &[BsonDocument], left: usize, right: usize, sort: &[(String, i32)]) -> Ordering {
    for (field, direction) in sort {
        let ordering = compare_values(&sort_key(&documents[left], field, *direction), &sort_key(&documents[right], field, *direction));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// `$sortByCount` over a `$`-prefixed field path.
///
/// The whole value is one group key, including an array or an empty array, and a
/// missing field groups as `null`, matching `mongod 8.0.9`. Expression objects
/// are a separate implementation and stay outside this gateway.
pub(crate) fn sort_by_count_stage(documents: &[BsonDocument], body: &BsonValue) -> Result<Vec<BsonDocument>, CommandError> {
    let path = match body {
        BsonValue::String(path) if path.starts_with('$') && path.len() > 1 => path[1..].to_string(),
        BsonValue::String(_) => {
            return Err(CommandError::new(
                40148,
                "the sortByCount field must be defined as a $-prefixed path or an expression inside an object",
            ));
        }
        BsonValue::Document(_) => {
            return Err(CommandError::new(
                59,
                "$sortByCount expressions are outside this gateway (coordinate: only `$`-prefixed field paths are supported)",
            ));
        }
        _ => {
            return Err(CommandError::new(40149, "the sortByCount field must be specified as a string or as an object"));
        }
    };

    let mut groups: Vec<(BsonValue, i32)> = Vec::new();
    let mut positions: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for document in documents {
        let value = lookup_field(document, &path).cloned().unwrap_or(BsonValue::Null);
        match positions.get(&value_to_key(&value)) {
            Some(position) => groups[*position].1 += 1,
            None => {
                positions.insert(value_to_key(&value), groups.len());
                groups.push((value, 1));
            }
        }
    }
    // Descending count, ties in first-appearance order. `mongod` leaves equal
    // counts in hash order, so equal counts are never asserted.
    groups.sort_by_key(|left| std::cmp::Reverse(left.1));
    Ok(groups
        .into_iter()
        .map(|(value, count)| {
            let mut grouped = BsonDocument::new();
            grouped.push("_id".to_string(), value);
            grouped.push("count".to_string(), count_value(i64::from(count)));
            grouped
        })
        .collect())
}

/// Accumulator kinds backed by a documented behavior sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccumulatorKind {
    Sum,
    Avg,
    Min,
    Max,
    Push,
    AddToSet,
    First,
    Last,
}

impl AccumulatorKind {
    fn parse(operator: &str) -> Option<Self> {
        match operator {
            "$sum" => Some(Self::Sum),
            "$avg" => Some(Self::Avg),
            "$min" => Some(Self::Min),
            "$max" => Some(Self::Max),
            "$push" => Some(Self::Push),
            "$addToSet" => Some(Self::AddToSet),
            "$first" => Some(Self::First),
            "$last" => Some(Self::Last),
            _ => None,
        }
    }
}

pub(crate) struct Accumulator {
    pub(crate) field: String,
    pub(crate) kind: AccumulatorKind,
    pub(crate) argument: BsonValue,
}

/// `$group` groups by `_id` (null, constant, or `"$field"`) and applies the
/// supported accumulators to each group. Output order is unspecified by the
/// manual, so callers must sort if they need one.
pub(crate) fn group_stage(documents: &[BsonDocument], specification: &BsonDocument) -> Result<Vec<BsonDocument>, CommandError> {
    let Some(identifier) = specification.get("_id")
    else {
        return Err(CommandError::new(2, "$group requires an `_id` expression"));
    };
    match identifier {
        BsonValue::Document(_) => {
            return Err(CommandError::new(
                59,
                "$group `_id` document expressions are not implemented; use null, a constant, or a \"$field\" path",
            ));
        }
        BsonValue::Array(_) => {
            return Err(CommandError::new(
                59,
                "$group `_id` array expressions are not implemented; use null, a constant, or a \"$field\" path",
            ));
        }
        _ => {}
    }

    let mut accumulators: Vec<Accumulator> = Vec::new();
    for (field, accumulator) in specification.fields.iter().filter(|(field, _)| field != "_id") {
        let BsonValue::Document(operator_document) = accumulator
        else {
            return Err(CommandError::new(2, format!("$group field `{field}` must be an accumulator document")));
        };
        if operator_document.fields.len() != 1 {
            return Err(CommandError::new(2, format!("$group field `{field}` must contain exactly one accumulator")));
        }
        let (operator, argument) = &operator_document.fields[0];
        let Some(kind) = AccumulatorKind::parse(operator)
        else {
            return Err(CommandError::new(
                59,
                format!("unknown accumulator: {operator} (supported: $sum, $avg, $min, $max, $push, $addToSet, $first, $last)"),
            ));
        };
        accumulators.push(Accumulator { field: field.clone(), kind, argument: argument.clone() });
    }

    // "If there are no documents from the prior pipeline stage, the $group stage
    // returns nothing."
    if documents.is_empty() {
        return Ok(Vec::new());
    }

    let mut groups: Vec<(BsonValue, Vec<usize>)> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let key = evaluate_group_id(document, identifier);
        match groups.iter_mut().find(|(existing, _)| values_equal(Some(existing), &key)) {
            Some((_, members)) => members.push(index),
            None => groups.push((key, vec![index])),
        }
    }

    let mut output = Vec::new();
    for (key, members) in groups {
        let mut grouped = BsonDocument::new();
        grouped.push("_id".to_string(), key);
        for accumulator in &accumulators {
            grouped.push(accumulator.field.clone(), accumulate(accumulator, documents, &members)?);
        }
        output.push(grouped);
    }
    Ok(output)
}

pub(crate) fn evaluate_group_id(document: &BsonDocument, identifier: &BsonValue) -> BsonValue {
    match identifier {
        BsonValue::String(path) if path.starts_with('$') => lookup_field(document, &path[1..]).cloned().unwrap_or(BsonValue::Null),
        other => other.clone(),
    }
}

pub(crate) fn accumulate(accumulator: &Accumulator, documents: &[BsonDocument], members: &[usize]) -> Result<BsonValue, CommandError> {
    // `$push` and `$addToSet` skip a missing field entirely while still keeping an
    // explicit `null`; every other accumulator treats missing as `null`.
    let optional_values: Vec<Option<BsonValue>> =
        members.iter().map(|index| evaluate_expression_optional(&documents[*index], &accumulator.argument)).collect::<Result<Vec<_>, _>>()?;
    let values: Vec<BsonValue> = optional_values.iter().cloned().map(|value| value.unwrap_or(BsonValue::Null)).collect();

    match accumulator.kind {
        AccumulatorKind::Sum => {
            let mut total: i128 = 0;
            let mut double_total: Option<f64> = None;
            for value in values {
                match value {
                    BsonValue::Int32(number) => total += i128::from(number),
                    BsonValue::Int64(number) => total += i128::from(number),
                    BsonValue::Double(number) => *double_total.get_or_insert(0.0) += number,
                    // "$sum ignores the non-numeric values"
                    _ => {}
                }
            }
            Ok(match double_total {
                Some(double) => BsonValue::Double(double + total as f64),
                None => match i64::try_from(total) {
                    Ok(exact) => count_value(exact),
                    Err(_) => BsonValue::Double(total as f64),
                },
            })
        }
        AccumulatorKind::Avg => {
            let mut total = 0.0_f64;
            let mut count = 0_u64;
            for value in values {
                if let Some(number) = number_of(&value) {
                    total += number;
                    count += 1;
                }
            }
            // "$avg returns null if all input values are non-numeric"
            Ok(if count == 0 { BsonValue::Null } else { BsonValue::Double(total / count as f64) })
        }
        AccumulatorKind::Min | AccumulatorKind::Max => {
            let wanted = if accumulator.kind == AccumulatorKind::Min { Ordering::Less } else { Ordering::Greater };
            let mut chosen: Option<BsonValue> = None;
            for value in values {
                // "$min/$max ignore null or missing values"
                if matches!(value, BsonValue::Null) {
                    continue;
                }
                chosen = Some(match chosen {
                    None => value,
                    Some(current) => {
                        if compare_values(&value, &current) == wanted {
                            value
                        }
                        else {
                            current
                        }
                    }
                });
            }
            Ok(chosen.unwrap_or(BsonValue::Null))
        }
        AccumulatorKind::Push => Ok(BsonValue::Array(optional_values.into_iter().flatten().collect())),
        AccumulatorKind::AddToSet => {
            let mut unique: Vec<BsonValue> = Vec::new();
            for value in optional_values.into_iter().flatten() {
                if !unique.iter().any(|existing| values_equal(Some(existing), &value)) {
                    unique.push(value);
                }
            }
            Ok(BsonValue::Array(unique))
        }
        AccumulatorKind::First => Ok(values.first().cloned().unwrap_or(BsonValue::Null)),
        AccumulatorKind::Last => Ok(values.last().cloned().unwrap_or(BsonValue::Null)),
    }
}

/// `$addFields`: keeps every input field and adds or replaces the specified
/// paths. Expressions are evaluated against the input document, so fields in
/// the same specification never observe each other.
pub(crate) fn add_fields_stage(documents: &[BsonDocument], specification: &BsonDocument) -> Result<Vec<BsonDocument>, CommandError> {
    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        let mut result = document.clone();
        for (field, expression) in &specification.fields {
            if field.starts_with('$') {
                return Err(CommandError::new(2, "FieldPath field names may not start with '$'. Consider using $getField or $setField."));
            }
            let value = evaluate_expression(document, expression)?;
            let segments: Vec<&str> = field.split('.').collect();
            set_aggregation_segments(&mut result, &segments, value)?;
        }
        output.push(result);
    }
    Ok(output)
}

pub(crate) fn project_stage(documents: &[BsonDocument], specification: &BsonDocument) -> Result<Vec<BsonDocument>, CommandError> {
    apply_projection(documents, &parse_projection(specification)?)
}

/// 按已校验的投影计划逐篇应用；表达式规则走聚合表达式求值。
pub(crate) fn apply_projection(documents: &[BsonDocument], plan: &ProjectionPlan) -> Result<Vec<BsonDocument>, CommandError> {
    if plan.is_exclusion() {
        let mut projected = Vec::new();
        for document in documents {
            let mut kept = document.clone();
            for (field, rule) in &plan.fields {
                if matches!(rule, ProjectionRule::Exclude) {
                    remove_field_path(&mut kept, field);
                }
            }
            if plan.excludes_id() {
                remove_field_path(&mut kept, "_id");
            }
            projected.push(kept);
        }
        return Ok(projected);
    }

    let mut projected = Vec::new();
    for document in documents {
        let mut result = BsonDocument::new();
        if !plan.excludes_id() {
            if let Some(expression) = plan.id_expression() {
                if let Some(value) = evaluate_expression_optional(document, expression)? {
                    result.push("_id".to_string(), value);
                }
            }
            else if let Some(identifier) = document.get("_id") {
                result.push("_id".to_string(), identifier.clone());
            }
        }
        for (field, rule) in &plan.fields {
            match rule {
                ProjectionRule::Include => {
                    if let Some(value) = lookup_field(document, field) {
                        set_field_path(&mut result, field, value.clone());
                    }
                }
                ProjectionRule::Exclude => {}
                ProjectionRule::Expression(expression) => {
                    let value = evaluate_expression(document, expression)?;
                    set_field_path(&mut result, field, value);
                }
            }
        }
        projected.push(result);
    }
    Ok(projected)
}

/// Minimal fixpoint helpers used by `$project`, `$unwind` and `$sort`.
pub(crate) fn set_field_path(document: &mut BsonDocument, path: &str, value: BsonValue) {
    let (head, tail) = match path.split_once('.') {
        Some((head, tail)) => (head, tail),
        None => {
            if let Some(entry) = document.fields.iter_mut().find(|(key, _)| key == path) {
                entry.1 = value;
            }
            else {
                document.push(path.to_string(), value);
            }
            return;
        }
    };
    if let Some(entry) = document.fields.iter_mut().find(|(key, _)| key == head)
        && let BsonValue::Document(inner) = &mut entry.1
    {
        set_field_path(inner, tail, value);
        return;
    }
    let mut nested = BsonDocument::new();
    set_field_path(&mut nested, tail, value);
    document.push(head.to_string(), BsonValue::Document(nested));
}

pub(crate) fn remove_field_path(document: &mut BsonDocument, path: &str) {
    match path.split_once('.') {
        None => document.fields.retain(|(key, _)| key != path),
        Some((head, tail)) => {
            if let Some(entry) = document.fields.iter_mut().find(|(key, _)| key == head)
                && let BsonValue::Document(inner) = &mut entry.1
            {
                remove_field_path(inner, tail);
            }
        }
    }
}
