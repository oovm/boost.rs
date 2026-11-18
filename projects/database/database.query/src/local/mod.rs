//! Local query layer: both frontends (VOS and SQL) lower into one shared
//! operator model. This crate owns the model, the parameter binding and the
//! plan; the operators themselves run in `database.execution`.
//!
//! | Stage | Module | Knows about |
//! |-------|--------|-------------|
//! | parse | `oak-vos` / `oak-sql` | authoring syntax |
//! | lower | `sql` / `lower` | frontend AST → `ops` |
//! | plan | `params` / `window` | parameters, kind, read/write set, record windows |
//!
//! Execution entry points live in `database_execution::local`.

use std::collections::BTreeMap;

use database_types::{TableUniqueConstraint, Value};
use schema_types::ResolvedContract;

#[doc(hidden)]
pub mod catalog;
#[doc(hidden)]
pub mod coerce;
#[doc(hidden)]
pub mod constraints;
mod dml;
#[doc(hidden)]
pub mod lower;
#[doc(hidden)]
pub mod ops;
mod params;
mod sql;
#[doc(hidden)]
pub mod store;
mod window;

pub use catalog::{resolved_contract_from_document, validate_contract_types};
pub use ops::{
    AggregateField, BoundValue, CmpOp, ConflictAction, ConflictClause, JoinKind, Pred, ProjectExpr, ProjectField, QueryOp, QueryRow,
    ReturningProjection, SessionOp, SetOpKind, SlotTy, SortKey, WindowField, WindowFrame, WindowFrameBound, WindowFunction, WindowOrderKey,
    WindowSpec, WriteAssignment, WriteField, WriteOp,
};
pub use params::{
    BoundStatement, BoundWrite, ParameterDescriptor, PreparedStatement, QueryKind, bind_named, bind_ordered, bind_vos_write, fingerprint, prepare,
    scan_and_rewrite,
};
#[doc(hidden)]
pub use params::{Dialect, prepare_sql};
pub use store::{
    BorrowedRow, BytesPlacementHook, ObjectResolver, ROW_PREFIX, RowReferences, RowSource, VectorPlacementHook, row_references, upsert_row,
    with_bytes_placement, with_object_resolver, with_vector_placement,
};
pub use window::{ReadWindow, read_window, write_windows};

/// Execute unit-valued VOS write programs (for example `User { … }.insert()`).
pub fn execute_write(
    source: &str,
    contract: &ResolvedContract,
    schema_document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> database_types::Result<()> {
    dml::execute(source, contract, schema_document, records)
}

/// Execute writes with host-declared uniqueness rules merged into document rules.
pub fn execute_write_with_constraints(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> database_types::Result<()> {
    dml::execute_with_constraints(source, contract, declared, schema_document, records)
}

/// Parse one VOS write program exactly once for both planning and execution.
pub fn parse_write_program(source: &str) -> database_types::Result<oak_vos::VosProgram> {
    dml::parse_write_program(source)
}

/// Execute an already parsed VOS write program.
pub fn execute_write_parsed(
    program: &oak_vos::VosProgram,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
    params: &[Value],
) -> database_types::Result<()> {
    dml::execute_parsed(program, contract, declared, schema_document, records, params)
}

/// Execute a VOS write program whose `__yydb_param_<index>` slots read `params`.
///
/// `source` is the rewritten program produced by [`bind_vos_write`]; the values
/// line up with the slot indices in that text.
pub fn execute_write_with_params(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    records: &mut BTreeMap<String, Vec<u8>>,
    params: &[Value],
) -> database_types::Result<()> {
    dml::execute_with_params(source, contract, declared, schema_document, records, params)
}

/// Record keys a point write must read before it runs.
///
/// Returns `None` when the program cannot be proven to be keyed writes on
/// rule-free tables, in which case the caller must take the full path.
pub fn point_write_keys(
    source: &str,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    params: &[Value],
    holds_every_row: bool,
) -> database_types::Result<Option<Vec<String>>> {
    dml::point_write_keys(source, contract, declared, schema_document, params, holds_every_row)
}

/// Same as [`point_write_keys`] over an already parsed program.
pub fn point_write_keys_parsed(
    program: &oak_vos::VosProgram,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    params: &[Value],
    holds_every_row: bool,
) -> database_types::Result<Option<Vec<String>>> {
    dml::point_write_keys_program(program, contract, declared, schema_document, params, holds_every_row)
}

/// Run a single `Type::insert({ … })` program and return the stored row when recognized.
pub fn try_insert_returning(
    source: &str,
    contract: &ResolvedContract,
    records: &mut BTreeMap<String, Vec<u8>>,
) -> database_types::Result<Option<Vec<QueryRow>>> {
    dml::try_insert_returning(source, contract, records)
}
