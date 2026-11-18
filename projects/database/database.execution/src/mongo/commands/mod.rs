//! 逐条命令原语。
//!
//! 每个函数都能被库使用者单独调用：没有隐藏的统一入口，也没有全局状态。宿主把
//! 这些原语组合成自己的 server 循环与 CLI。
//!
//! 按职责拆为子模块：表达式求值、读/写命令、索引与 TTL、explain 装配、
//! 会话与事务、目录与统计。公共路径仍是 `mongo::commands::*`。

pub(crate) use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) use database_protocol_mongo::bson::{
    BinaryData, BsonDocument, BsonValue, encode_bson, integral_value, parse_bson, parse_bson_projected, value_to_key,
};

pub(crate) use database_protocol_mongo::{decimal128::Decimal128, json};

pub(crate) use database_query::mongo::{
    ExpressionContext, ExpressionVariables, Hint, HintedPlan, IndexCatalog, PipelineStage, ProjectionPlan, ProjectionRule,
    aggregate_stop_after, automatic_candidates, build_hinted_plan, collect_field, compare_values, count_value, equality_candidates,
    expression_truthy, has_operator, hex_text, hinted_count_plan, hinted_find_plan, index_executable, index_tuple, int_value, integer_of,
    integral_i64, lookup_field, matches, matches_with_variables, multi_key_fields, normalize_query, number_of, operator_matches, parse_count,
    parse_projection, parse_sort_stage, plan::IndexSpan, plan_pipeline, plan_spans, positive_stage_integer, references_variables,
    render_shell_value, scan_stage_document, unsupported_stage, validate_count_name, validate_filter, values_equal, winning_plan,
};

pub(crate) use crate::mongo::{
    cursor::{CursorScan, CursorStore, DEFAULT_FIRST_BATCH, GetMoreError},
    id::generate_object_id,
    oplog::OplogEntry,
    storage::{
        CollectionOptions, IndexSpec, MongoStorage, NamespaceWrite, SessionWriteState, TransactionAbort, TransactionBegin, TransactionCommit,
        TransactionPage, TransactionWrite,
    },
};

pub use database_query::mongo::CommandError;

mod catalog;
mod explain;
mod expression;
mod index;
mod pipeline;
mod read;
mod session;
mod support;
mod write;

pub(crate) use self::expression::*;
pub use self::{catalog::*, explain::*, index::*, pipeline::*, read::*, session::*, support::*, write::*};
