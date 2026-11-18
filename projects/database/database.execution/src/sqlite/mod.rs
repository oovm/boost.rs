//! SQLite dialect evaluation mechanism shared by the product facades.
//!
//! The resolved syntax shapes live in `database_query::sqlite`; this module
//! tree owns their evaluation: expression and cell mechanisms, the scalar
//! function library, the JSON and datetime function families, the host UDF
//! bridge, and the statement-level SELECT machinery in [`select`] with its
//! index probes ([`probe`]) and window pass ([`window`]).

pub mod cells;
pub mod datetime;
pub mod eval;
pub mod host;
pub mod json;
pub mod probe;
pub mod scalar;
pub mod select;
pub mod window;
