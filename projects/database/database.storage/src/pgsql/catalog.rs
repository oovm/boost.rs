//! PostgreSQL `.yyds` catalog document and the `postmaster.pid` directory lock.
//!
//! The catalog file names the single shard a PostgreSQL data directory
//! registers, and `postmaster.pid` records the process that owns the directory.
//! Both layouts are product contracts: the catalog bytes come from
//! [`database_formats::yy::yyds`], while the messages PostgreSQL users already
//! see for a legacy or unusable catalog stay here.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use database_formats::yy::yyds::{CATALOG_MAGIC, decode_catalog_shard_id as shared_catalog_shard_id, encode_catalog};
use database_types::cluster::{Error as ClusterError, ShardId};

use super::Error;

/// Shard id a fresh data directory registers.
pub const DEFAULT_SHARD_ID: &str = "local-0";

/// Catalog file written next to the shard file.
pub const CATALOG_FILE: &str = "local-0.yyds";

/// Catalog file name of the pre-contract big-endian layout, rejected outright.
pub const LEGACY_CATALOG_FILE: &str = "postgres-0.yyds";

/// Lock file the server creates while it owns the data directory.
pub const LOCK_FILE: &str = "postmaster.pid";

fn io_error(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

fn protocol(message: &str) -> Error {
    Error::Corrupt(message.to_owned())
}

/// Resolves the shard id a catalog registers, or the default when none exists.
pub fn catalog_shard_id(catalog: &Path) -> Result<String, Error> {
    if !catalog.exists() {
        return Ok(DEFAULT_SHARD_ID.to_owned());
    }
    let bytes = fs::read(catalog).map_err(io_error)?;
    decode_catalog_shard_id(&bytes)
}

/// Encodes a catalog registering exactly `shard_id`.
pub fn encode_catalog_bytes(shard_id: &str) -> Result<Vec<u8>, Error> {
    let message = match encode_catalog(None, None, &[ShardId(shard_id.to_owned())], None, &[]) {
        Ok(bytes) => return Ok(bytes),
        Err(error) => format!("cannot encode .yyds catalog: {error}"),
    };
    Err(protocol(&message))
}

/// Reads the single shard id a catalog registers.
///
/// Decoding and single-registration validation come from `database-formats`;
/// this wrapper only keeps the PostgreSQL-facing wording and the legacy
/// big-endian pre-check.
pub fn decode_catalog_shard_id(bytes: &[u8]) -> Result<String, Error> {
    if legacy_catalog_bytes(bytes) {
        return Err(protocol("legacy PostgreSQL catalog layout: delete the store directory and reinitialize"));
    }
    let shard_id = shared_catalog_shard_id(bytes).map_err(catalog_error)?;
    // `ShardId::try_new` rejects `.`, `..` and path separators. The id also
    // becomes a file stem here, so a NUL byte stays unusable even though the
    // shared constructor does not check for it.
    if shard_id.0.as_bytes().contains(&0) {
        return Err(protocol("catalog shard id is unusable"));
    }
    Ok(shard_id.0)
}

fn catalog_error(error: ClusterError) -> Error {
    match error {
        ClusterError::InvalidShardId { .. } => protocol("catalog shard id is unusable"),
        ClusterError::Unsupported(reason) if reason == "catalog registers no shard" || reason == "catalog registers more than one shard" => {
            protocol("unsupported catalog shard count")
        }
        other => protocol(&format!("invalid .yyds catalog: {}", catalog_reason(other))),
    }
}

fn catalog_reason(error: ClusterError) -> String {
    match error {
        ClusterError::Corrupt(reason) | ClusterError::Unsupported(reason) => reason.to_owned(),
        other => other.to_string(),
    }
}

/// Pre-contract PostgreSQL catalogs encoded the shard count as big-endian
/// `1` where the contract stores the routing presence flag. They are rejected
/// outright; the bytes are never interpreted as a little-endian catalog.
fn legacy_catalog_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 17
        && bytes.starts_with(CATALOG_MAGIC)
        && bytes[5..9] == [0, 0, 0, 0]
        && bytes[9..13] == [0, 0, 0, 0]
        && bytes[13..17] == [0, 0, 0, 1]
}

/// Publishes the catalog through the storage publisher.
///
/// The commit point, replacement strategy and failure semantics are the ones
/// [`crate::local::publish::publish_atomic`] already proves for database files:
/// temp write, one directory-entry replace, backup restore when the destination
/// is busy, parent sync. The PostgreSQL catalog does not keep a private
/// `.tmp → rename` sequence of its own.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    crate::local::publish::publish_atomic(path, bytes).map_err(|error| io_error(format!("{}: {error}", path.display())))
}

/// Takes the `postmaster.pid` lease of `dir` for the current process.
///
/// The lock file is product-side instead of the upstream
/// `database-storage::local::lock::WriterLock`. The three observable contracts
/// differ:
///
/// 1. File content: this lock is `postmaster.pid` holding the live PID and a
///    newline, while `WriterLock` writes a `{db}-lock` sidecar with a `YYLK`
///    magic, a fence epoch and a CRC32.
/// 2. Error wording: a second server must fail with `lock file "..." already
///    exists: another server owns this data directory`, which PostgreSQL
///    callers and tests match verbatim.
/// 3. Crash recovery: a hard kill leaves `postmaster.pid` behind on purpose, so
///    the operator removes it once the old process is gone. `WriterLock`
///    releases its OS lock automatically when the process dies.
pub(crate) fn take_lock(dir: &Path) -> Result<PathBuf, Error> {
    let lock = dir.join(LOCK_FILE);
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&lock) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::Locked { path: lock });
        }
        Err(error) => return Err(io_error(error)),
    };
    let line = format!("{}\n", std::process::id());
    file.write_all(line.as_bytes()).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    Ok(lock)
}

/// Releases one directory lease, keeping the rest of the directory in place.
pub(crate) fn drop_lock(lock: &Path) {
    let _ = fs::remove_file(lock);
}
