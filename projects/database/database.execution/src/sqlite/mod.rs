//! SQLite dialect evaluation mechanism shared by the product facades.
//!
//! The resolved syntax shapes live in `database_query::sqlite`; this module
//! tree owns their evaluation: expression and cell mechanisms, the scalar
//! function library, the JSON and datetime function families, the host UDF
//! bridge, and the statement-level SELECT machinery in [`select`] with its
//! index probes ([`probe`]) and window pass ([`window`]).
//!
//! The statement-level DML machinery ([`dml`]) writes rows for the product
//! facades and the constraint guards ([`guards`]) validate uniqueness,
//! foreign keys and `CHECK` constraints against the same read view. The DDL
//! and schema-persistence machinery ([`ddl`]) evolves the catalog and the
//! stored `sqlite_master` text for the facades.

pub mod cells;
pub mod datetime;
pub mod ddl;
pub mod dml;
pub mod eval;
pub mod guards;
pub mod host;
pub mod json;
pub mod probe;
pub mod scalar;
pub mod select;
pub mod trigger;
pub mod view;
pub mod window;
