//! PostgreSQL data-directory storage: catalog document, `.yykv` shard, the
//! `postmaster.pid` lease and the table-group shard layout.
//!
//! The layout is PostgreSQL product behaviour, not transport detail, so it is
//! owned here instead of in a facade or a binding. The remote client keeps
//! connections only and never opens a local data directory.

/// `.yyds` catalog document and the `postmaster.pid` directory lock.
pub mod catalog;
/// One server data directory: catalog, shard and directory lease.
pub mod store;
/// Durable namespace/key store over one catalog and one shard.
pub mod shard;

pub use catalog::{CATALOG_FILE, DEFAULT_SHARD_ID, LEGACY_CATALOG_FILE, LOCK_FILE};
pub use shard::Store;
pub use store::{ServerChange, ServerRow, ServerStore};
