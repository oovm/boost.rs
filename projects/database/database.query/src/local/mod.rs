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
#[doc(hidden)]
pub mod placement;
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
    BoundStatement, BoundWrite, ParameterDescriptor, PreparedStatement, QueryKind, bind_named, bind_ordered, bind_vos_write, fingerprint, prepare,
    scan_and_rewrite,
};
#[doc(hidden)]
pub use params::{Dialect, prepare_sql};
pub use placement::{BytesPlacementHook, VectorPlacementHook, with_bytes_placement, with_vector_placement};
pub use window::{ReadWindow, read_window, write_windows};

