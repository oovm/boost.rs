//! Local query layer: both frontends (VOS and SQL) lower into one shared
//! operator model. This crate owns the model, the parameter binding and the
//! plan; the operators themselves run in `database.execution`.
//!
//! | Stage | Module | Knows about |
//! |-------|--------|-------------|
//! | parse | `data-text-vos` / `data-text-sql` | authoring syntax |
//! | lower | `sql` / `lower` | frontend AST → `ops` |
//! | plan | `params` / `window` | parameters, kind, read/write set, record windows |
//!
//! Execution entry points live in `database_execution::local`.

#[doc(hidden)]
pub mod catalog;
#[doc(hidden)]
pub mod coerce;
#[doc(hidden)]
pub mod constraints;
#[doc(hidden)]
pub mod lower;
#[doc(hidden)]
pub mod ops;
#[doc(hidden)]
pub mod params;
mod sql;
mod window;
#[doc(hidden)]
pub mod write_plan;

pub use catalog::{resolved_contract_from_document, validate_contract_types};
pub use ops::{
    AggregateField, BoundValue, CmpOp, ConflictAction, ConflictClause, JoinKind, Pred, ProjectExpr, ProjectField, QueryOp, QueryRow,
    ReturningProjection, SessionOp, SetOpKind, SlotTy, SortKey, WindowField, WindowFrame, WindowFrameBound, WindowFunction, WindowOrderKey,
    WindowSpec, WriteAssignment, WriteField, WriteOp,
};
pub use params::{
    BoundStatement, BoundWrite, ParameterDescriptor, ParsedVosWrite, PreparedStatement, QueryKind, bind_named, bind_ordered, bind_vos_write,
    fingerprint, parse_vos_write, prepare, scan_and_rewrite,
};
#[doc(hidden)]
pub use params::{Dialect, prepare_sql};
pub use window::{ReadWindow, read_window, write_windows};

