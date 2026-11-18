//! MongoDB 聚合管道计划：stage 解析、顺序合法性与计划构造。
//!
//! 计划只判断「这条管道能不能执行」，不执行任何 stage；`$match` 下推边界、
//! `$lookup` 子管道与 `$facet` 分支都由执行层按同一份计划驱动。

use database_protocol_mongo::bson::{BsonDocument, BsonValue};

use crate::mongo::{CommandError, value::int_value};

/// 解析后的一段管道 stage。
#[derive(Debug, Clone)]
pub struct PipelineStage {
    /// stage 名（含 `$`）。
    pub name: String,
    /// stage 体，保持请求里的原样。
    pub body: BsonValue,
}

/// 本网关认识的 stage 名，未列出的按 `59` 拒绝。
pub const SUPPORTED_STAGES: [&str; 26] = [
    "$match",
    "$sort",
    "$limit",
    "$skip",
    "$count",
    "$unwind",
    "$lookup",
    "$graphLookup",
    "$facet",
    "$bucket",
    "$bucketAuto",
    "$setWindowFields",
    "$sortByCount",
    "$group",
    "$addFields",
    "$set",
    "$project",
    "$replaceWith",
    "$merge",
    "$out",
    "$unionWith",
    "$sample",
    "$densify",
    "$fill",
    "$redact",
    "$documents",
];

/// `$documents` 只在 `{aggregate: 1}` 形态下合法，执行层负责报这个错误。
pub const DOCUMENTS_STAGE: &str = "$documents";

/// Unknown aggregation stage: the command answers `59`.
pub fn unsupported_stage(stage: &str) -> CommandError {
    CommandError::new(
        59,
        format!(
            "unknown aggregation stage: {stage} (supported: $match, $sort, $limit, $skip, $count, $unwind, $lookup, $graphLookup, $facet, $bucket, $bucketAuto, $setWindowFields, $sortByCount, $group, $addFields, $set, $project, $replaceWith, $merge, $out, $unionWith, $sample, $densify, $fill, $redact)"
        ),
    )
}

/// Parses and validates one pipeline before any stage runs.
///
/// A stage must be a one-field document, `$out` / `$merge` must be last, and every
/// name must be in [`SUPPORTED_STAGES`]. Ordering is checked here so a bad pipeline
/// fails before an earlier stage mutates the collection.
pub fn plan_pipeline(stages: &[BsonValue]) -> Result<Vec<PipelineStage>, CommandError> {
    let mut planned = Vec::with_capacity(stages.len());
    for (position, stage) in stages.iter().enumerate() {
        let BsonValue::Document(stage_document) = stage
        else {
            return Err(CommandError::new(2, "aggregate stage must be a document"));
        };
        if stage_document.fields.len() != 1 {
            return Err(CommandError::new(2, "aggregate stage must contain exactly one field"));
        }
        let (name, body) = &stage_document.fields[0];
        if !SUPPORTED_STAGES.contains(&name.as_str()) {
            return Err(unsupported_stage(name));
        }
        if matches!(name.as_str(), "$out" | "$merge") && position + 1 != stages.len() {
            return Err(CommandError::new(40601, format!("{name} can only be the final stage in the pipeline")));
        }
        planned.push(PipelineStage { name: name.clone(), body: body.clone() });
    }
    Ok(planned)
}

/// 首段 `$match` 之后紧跟 `$limit`（或 `$skip` + `$limit`）时的扫描边界。
///
/// 只看紧邻的这两段：中间还有别的阶段时，`$limit` 前面的文档数无法从访问路径
/// 直接推出，不能提前停。
pub fn aggregate_stop_after(stages: &[PipelineStage]) -> Option<usize> {
    let first = stages.first()?;
    match first.name.as_str() {
        "$limit" => positive_stage_integer("$limit", &first.body).ok(),
        "$skip" => {
            let skip = usize::try_from(int_value(&first.body)?).ok()?;
            let next = stages.get(1)?;
            if next.name == "$limit" { Some(skip.saturating_add(positive_stage_integer("$limit", &next.body).ok()?)) } else { None }
        }
        _ => None,
    }
}

/// `$limit` / `$skip` take a positive integer.
pub fn positive_stage_integer(stage: &str, value: &BsonValue) -> Result<usize, CommandError> {
    match int_value(value) {
        Some(number) if number > 0 => Ok(number as usize),
        _ => Err(CommandError::new(2, format!("{stage} requires a positive integer"))),
    }
}

/// `$count` output field name: non-empty, no leading `$`, no `.`.
pub fn validate_count_name(name: &str) -> Result<(), CommandError> {
    if name.is_empty() || name.starts_with('$') || name.contains('.') {
        return Err(CommandError::new(
            2,
            format!("$count output field name `{name}` must be non-empty, must not start with `$` and must not contain `.`"),
        ));
    }
    Ok(())
}

/// `$sort` accepts 1/-1 directions, rejects duplicates and caps the key count at 32.
pub fn parse_sort_stage(specification: &BsonDocument) -> Result<Vec<(String, i32)>, CommandError> {
    if specification.fields.is_empty() {
        return Err(CommandError::new(2, "$sort requires at least one field"));
    }
    if specification.fields.len() > 32 {
        return Err(CommandError::new(2, "$sort accepts at most 32 keys"));
    }
    let mut sort: Vec<(String, i32)> = Vec::new();
    for (field, direction) in specification.fields.iter() {
        if sort.iter().any(|(existing, _)| existing == field) {
            return Err(CommandError::new(2, format!("$sort field `{field}` appears more than once")));
        }
        match int_value(direction) {
            Some(value) if value == 1 || value == -1 => sort.push((field.clone(), value as i32)),
            _ => return Err(CommandError::new(2, format!("$sort direction for `{field}` must be 1 or -1"))),
        }
    }
    Ok(sort)
}

/// 值里是否读取了聚合变量（`$$名称`）。
///
/// 只做保守判断：任何以 `$$` 开头的字符串都算引用，宁可不下推也不改语义。
pub fn references_variables(value: &BsonValue) -> bool {
    match value {
        BsonValue::String(text) => text.starts_with("$$"),
        BsonValue::Array(items) => items.iter().any(references_variables),
        BsonValue::Document(document) => document.fields.iter().any(|(_, value)| references_variables(value)),
        _ => false,
    }
}
