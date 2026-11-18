//! MongoDB 查询模型：过滤、投影、索引选择与聚合管道计划。
//!
//! 本模块只做无状态的解析、校验与计划构造，不访问存储、不持有会话状态。
//! 扫描、求值驱动与命令执行归 `database.execution::mongo`。

pub mod filter;
pub mod value;

pub use filter::{
    ExpressionContext, ExpressionVariables, FILTER_OPERATORS, LOGICAL_OPERATORS, collect_field, equality_candidates, has_operator, matches,
    matches_with_variables, normalize_query, operator_matches, validate_filter,
};
pub use value::{
    compare_values, count_value, expression_truthy, int_value, integer_of, integral_i64, lookup_field, number_of, parse_count, value_rank,
    values_equal,
};

use database_formats::mongo::StoreError;
use database_protocol_mongo::bson::BsonValue;

/// Command failure carrying the MongoDB error code.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandError {
    /// MongoDB error code (`59` unsupported, `73` invalid namespace, `43` no such cursor).
    pub code: i32,
    /// Message drivers surface as `errmsg`.
    pub message: String,
    /// Extra top-level fields the real reply carries after `codeName`, such as
    /// `keyPattern` / `keyValue` on an index-build duplicate key failure.
    pub details: Vec<(String, BsonValue)>,
}

impl CommandError {
    /// Builds one command failure.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), details: Vec::new() }
    }

    /// Appends one top-level field to the error reply.
    pub fn with_detail(mut self, key: &str, value: BsonValue) -> Self {
        self.details.push((key.to_string(), value));
        self
    }

    /// `CommandNotFound`: the command or stage is outside the documented surface.
    pub fn unsupported(stage: &str) -> Self {
        Self::new(59, stage)
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CommandError {}

/// A storage failure is a server-side failure, never a client mistake and never
/// "not found". It is surfaced as `InternalError` (`1`) with the backend reason
/// kept verbatim so a broken store cannot look like an empty collection.
impl From<StoreError> for CommandError {
    fn from(error: StoreError) -> Self {
        Self::new(1, format!("storage failure: {error}"))
    }
}
