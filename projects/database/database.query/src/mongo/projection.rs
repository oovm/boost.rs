//! MongoDB 投影：模式判定与合法性校验。
//!
//! 一个投影要么是包含投影（只列出要保留的字段），要么是排除投影（列出要丢弃的
//! 字段），模式由第一个非 `_id` 字段固定。校验在读取文档之前完成，执行侧只按
//! 计划应用，不再各写一套判定。

use database_protocol_mongo::bson::{BsonDocument, BsonValue};

use crate::mongo::CommandError;

/// 投影模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    /// 只保留列出的字段与 `_id`。
    Inclusion,
    /// 丢弃列出的字段，其余保留。
    Exclusion,
}

/// 一个字段的投影规则。
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionRule {
    /// 保留该字段。
    Include,
    /// 丢弃该字段。
    Exclude,
    /// 用聚合表达式计算该字段。
    Expression(BsonValue),
}

/// 已校验的投影计划。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionPlan {
    /// 由第一个非 `_id` 字段决定的模式。
    pub mode: ProjectionMode,
    /// 非 `_id` 字段的规则，保持请求顺序。
    pub fields: Vec<(String, ProjectionRule)>,
    /// `_id` 的规则，缺省时按包含模式处理。
    pub id: Option<ProjectionRule>,
}

impl ProjectionPlan {
    /// `true` 表示按排除模式应用。
    pub fn is_exclusion(&self) -> bool {
        self.mode == ProjectionMode::Exclusion
    }

    /// `_id` 被显式排除。
    pub fn excludes_id(&self) -> bool {
        matches!(self.id, Some(ProjectionRule::Exclude))
    }

    /// `_id` 的计算表达式（布尔规则之外的值）。
    pub fn id_expression(&self) -> Option<&BsonValue> {
        match &self.id {
            Some(ProjectionRule::Expression(expression)) => Some(expression),
            _ => None,
        }
    }
}

/// A numeric or boolean `1` in a projection rule.
pub fn rule_is_inclusion(value: &BsonValue) -> bool {
    matches!(value, BsonValue::Int32(1) | BsonValue::Int64(1) | BsonValue::Bool(true))
        || matches!(value, BsonValue::Double(number) if *number == 1.0)
}

/// A numeric or boolean `0` in a projection rule.
pub fn rule_is_exclusion(value: &BsonValue) -> bool {
    matches!(value, BsonValue::Int32(0) | BsonValue::Int64(0) | BsonValue::Bool(false))
        || matches!(value, BsonValue::Double(number) if *number == 0.0)
}

/// Parses one projection document into an executable plan.
///
/// The projection mode is fixed by the first non-`_id` field, and a later field of
/// the other mode is rejected with the same code and message as `mongod 8.0.9`.
/// Boolean/numeric `_id` rules never count as a conflict.
pub fn parse_projection(specification: &BsonDocument) -> Result<ProjectionPlan, CommandError> {
    let mut exclusion_projection: Option<bool> = None;
    for (field, rule) in specification.fields.iter() {
        if field == "_id" && (rule_is_exclusion(rule) || rule_is_inclusion(rule)) {
            continue;
        }
        let is_exclusion = rule_is_exclusion(rule);
        match exclusion_projection {
            None => exclusion_projection = Some(is_exclusion),
            Some(true) if !is_exclusion => {
                if rule_is_inclusion(rule) {
                    return Err(CommandError::new(
                        31253,
                        format!("Invalid $project :: caused by :: Cannot do inclusion on field {field} in exclusion projection"),
                    ));
                }
                return Err(CommandError::new(
                    31252,
                    "Invalid $project :: caused by :: Cannot use expression other than $meta in exclusion projection".to_string(),
                ));
            }
            Some(false) if is_exclusion => {
                return Err(CommandError::new(
                    31254,
                    format!("Invalid $project :: caused by :: Cannot do exclusion on field {field} in inclusion projection"),
                ));
            }
            _ => {}
        }
    }
    let mode = if exclusion_projection.unwrap_or(false) { ProjectionMode::Exclusion } else { ProjectionMode::Inclusion };
    let fields =
        specification.fields.iter().filter(|(field, _)| field != "_id").map(|(field, rule)| (field.clone(), classify_rule(rule))).collect();
    Ok(ProjectionPlan { mode, fields, id: specification.get("_id").map(classify_rule) })
}

fn classify_rule(rule: &BsonValue) -> ProjectionRule {
    if rule_is_exclusion(rule) {
        ProjectionRule::Exclude
    }
    else if rule_is_inclusion(rule) {
        ProjectionRule::Include
    }
    else {
        ProjectionRule::Expression(rule.clone())
    }
}
