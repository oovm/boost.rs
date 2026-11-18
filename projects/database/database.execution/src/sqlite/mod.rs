//! SQLite dialect evaluation mechanism shared by the product facades.
//!
//! The resolved syntax shapes live in `database_query::sqlite`; this module
//! tree owns their evaluation: expression and cell mechanisms, the scalar
//! function library, the JSON and datetime function families, the host UDF
//! bridge and the [`eval::SqliteSubqueryRun`] seam back to statement execution.

pub mod cells;
pub mod datetime;
pub mod eval;
pub mod host;
pub mod json;
pub mod scalar;
