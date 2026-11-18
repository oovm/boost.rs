//! `database-query` owns binding, parameters, predicates, projections, plan validation and result description.
#![deny(missing_docs)]

pub mod catalog_bind;
pub mod cluster_kv;
pub mod ir;
pub mod local;
pub mod mongo;
pub mod pgsql;
pub mod sql;
pub mod sql_batch;
pub mod sql_ddl;
pub mod sql_describe;
pub mod sql_execute;
pub mod sql_index;
pub mod sql_meta;
pub mod sql_parameters;
pub mod sql_planner;
pub mod sql_schema;
pub mod sqlite;

pub use catalog_bind::{
    SqlCatalog, SqlCatalogBoundColumn, SqlCatalogBoundExpression, SqlCatalogBoundJoin, SqlCatalogBoundProjection, SqlCatalogBoundSelect,
    SqlCatalogBoundSetOperand, SqlCatalogBoundSetOperation, SqlCatalogBoundStatement, SqlCatalogBoundTable, SqlCatalogField,
    SqlCatalogForeignKey, SqlCatalogOuterColumn, SqlCatalogTable, SqlFieldType, bind_sql_catalog, bind_sql_catalog_with_variables,
    project_resolved_type,
};
pub use pgsql::{
    CopyFormat, FetchDirection, PgObjectStatement, PgUtilityStatement, parse_pg_object_statement, parse_pg_utility,
    rewrite_double_colon_casts, rewrite_escape_strings, rewrite_pg_temp_references,
};
pub use sql::{
    DiscardTarget, MYSQL_BLOB_CAPACITY, SavepointIntent, SqlBoundBinaryOperator, SqlBoundConflict, SqlBoundConflictAction, SqlBoundCte,
    SqlBoundExpression, SqlBoundJoin, SqlBoundJoinKind, SqlBoundLiteral, SqlBoundProjection, SqlBoundSelect, SqlBoundSetOperand,
    SqlBoundSetOperation, SqlBoundSetOperator, SqlBoundSource, SqlBoundStatement, SqlBoundUnaryOperator, SqlDialect, SqlErrorKind,
    SqlFrontendError, SqlSessionCommand, SqlTransactionAction, SqlVariableScope, bind_sql, detect_savepoint_statement,
    detect_session_reset_statement, encode_hex, normalize_identifier, normalize_reserved_function_calls, parse_session_command, parse_sql,
};
pub use sql_batch::{SqlBatchStatement, split_sql_batch};
pub use sql_ddl::{
    ACCEPTED_COLLATIONS, DdlColumn, DdlForeignKey, DdlReference, DdlTableConstraint, OnCommit, SqlDdlStatement, is_accepted_collation,
    is_ddl_statement, is_pg_temp_schema, parse_ddl,
};
pub use sql_describe::{SqlColumn, describe_bound_insert, describe_plan, describe_plan_for_dialect};
pub use sql_execute::{RowFieldValues, SqlResult, SqlValue, decode_row, encode_row};
pub use sql_meta::{
    MetaQuery, PG_SERVER_VERSION, analyze_meta_query, mysql_data_type, pg_server_version, postgres_type, set_pg_server_version,
};
pub use sql_parameters::{SqlParameterStyle, neutralized_sql_parameters, nulled_sql_parameters, substitute_sql_parameters};
pub use sql_planner::{
    DefaultValue, DeletePlan, GeneratedColumnPlan, IndexProbe, InsertSelectColumn, InsertSelectPlan, JoinPlan, JoinSourcePlan, ReadPlan,
    ScanPlan, SetOperationPlan, SqlExecutionPlan, SqlSetOrderByItem, UniqueConstraintPlan, UpdateAssignment, UpdatePlan, WritePlan,
    plan_select, plan_select_for_dialect, plan_statement, plan_statement_for_dialect,
};
pub use sql_schema::{SqlAutoIncrementField, SqlDdlOutcome, SqlSchemaCheckpoint, SqlSchemaRegistry, apply_auto_increment};

/// 方言伪装响应使用的版本标签（`database-0.1.0`）。
pub fn version_label() -> String {
    format!("database-{}", database_types::cluster::version())
}
