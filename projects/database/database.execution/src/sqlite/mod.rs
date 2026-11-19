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
//! stored `sqlite_master` text for the facades. The read-only statement
//! metadata machinery ([`describe`]), the schematic `EXPLAIN` rows ([`explain`])
//! and the catalog-shaped `PRAGMA` rows ([`pragma`]) derive their result sets
//! from the same read view without executing anything.

pub mod cells;
pub mod cursor;
pub mod datetime;
pub mod ddl;
pub mod describe;
pub mod dml;
/// 连接级执行会话：引擎状态、附加库递归会话与语句分发。
pub mod engine;
pub mod eval;
pub mod explain;
pub mod guards;
pub mod host;
pub mod json;
pub mod pragma;
pub mod probe;
pub mod scalar;
pub mod select;
/// SQLite 连接级会话状态机：脏镜像跟踪、事务代数、发布边界判定与游标表。
pub mod session;
pub mod stmt;
pub mod trigger;
pub mod txn;
pub mod view;
pub mod window;
