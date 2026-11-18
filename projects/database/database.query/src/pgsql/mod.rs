//! PostgreSQL dialect front-end rewrites applied before the shared Oak parser.
//!
//! Oak's SQL surface lexes the PostgreSQL `::` cast operator but does not build
//! a cast node, so the suffix would be dropped at bind time. Instead of keeping
//! a second generic parser, this module rewrites the operator into the `CAST`
//! form Oak already models, and the shared binder/planner/executor path runs
//! unchanged.

pub mod casts;
pub mod utility;

pub use casts::rewrite_double_colon_casts;
pub use utility::{CopyFormat, FetchDirection, PgUtilityStatement, parse_pg_utility};
