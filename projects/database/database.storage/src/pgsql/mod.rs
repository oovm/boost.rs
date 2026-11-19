//! PostgreSQL data-directory storage: catalog document, `.yykv` shard, the
//! `postmaster.pid` lease and the table-group shard layout.
//!
//! The layout is PostgreSQL product behaviour, not transport detail, so it is
//! owned here instead of in a facade or a binding. The remote client keeps
//! connections only and never opens a local data directory.

/// `.yyds` catalog document and the `postmaster.pid` directory lock.
pub mod catalog;
/// Storage-owned failures of the data directory, catalog and lease.
mod error;
/// One server data directory: catalog, shard and directory lease.
pub mod store;
/// Durable namespace/key store over one catalog and one shard.
pub mod shard;
/// One directory lease plus the `shards/<shard-id>/` table-group layout.
pub mod instance;
/// Databases, roles and table grants, persisted beside the schema.
pub mod objects;

pub use error::Error;

pub use catalog::{CATALOG_FILE, DEFAULT_SHARD_ID, LEGACY_CATALOG_FILE, LOCK_FILE};
pub use instance::{
    ServerInstance, SHARD_DIRECTORY_FILES, SHARD_ROOT, clear_stale_lock, process_exists, shard_directory, validate_shard_id,
};
pub use shard::Store;
pub use store::{ServerChange, ServerRow, ServerStore};
pub use objects::{
    OBJECTS_CONTRACT, OBJECTS_FILE, ObjectError, PgDatabase, PgGrant, PgMembership, PgObjectCatalog, PgPassword, PgPrivilege, PgRole,
    PgScramVerifier, PgSequence, PgSequenceAlteration,
};
