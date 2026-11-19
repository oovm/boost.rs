//! One PostgreSQL data directory: the `.yyds` catalog, the `.yykv` shard and
//! the `postmaster.pid` lease.
//!
//! [`ServerStore`] is the single owner of a server data directory. Every
//! mutation of one statement goes through one [`ServerStore::commit_changes`]
//! call, which is [`crate::cluster_kv::FileShard::commit_changes`]: it stages
//! the complete post-commit snapshot, writes it durably (temp file,
//! `sync_all`, atomic replace) and only then publishes it to readers. A failure
//! leaves the previous snapshot intact.
//!
//! The data directory also carries a `postmaster.pid` lock file. Opening a
//! directory that another live process locked fails instead of silently sharing
//! the shard. A stale lock from a crashed process is reclaimed by
//! [`crate::pgsql::instance::clear_stale_lock`] before the open.

use std::{
    fs,
    path::{Path, PathBuf},
};

use database_types::cluster::{Error as ClusterError, Namespace};

use crate::cluster_kv::{FileShard, InlineValue, Key, KvStore, ShardChange, StoredValue};

use super::Error;
use super::catalog::{CATALOG_FILE, LEGACY_CATALOG_FILE, LOCK_FILE, catalog_shard_id, drop_lock, encode_catalog_bytes, take_lock, write_atomic};

fn io_error(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

fn protocol(message: &str) -> Error {
    Error::Corrupt(message.to_owned())
}

/// One stored row returned by [`ServerStore::scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRow {
    /// Row key bytes inside the queried namespace.
    pub key: Vec<u8>,
    /// Inline value bytes.
    pub value: Vec<u8>,
}

/// One mutation inside a [`ServerStore::commit_changes`] batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerChange {
    /// Insert or replace one row.
    Put {
        /// Namespace the row belongs to.
        namespace: String,
        /// Row key bytes.
        key: Vec<u8>,
        /// Inline value bytes.
        value: Vec<u8>,
    },
    /// Remove one row when present.
    Delete {
        /// Namespace the row belongs to.
        namespace: String,
        /// Row key bytes.
        key: Vec<u8>,
    },
}

impl ServerChange {
    fn into_shard_change(self) -> ShardChange {
        match self {
            Self::Put { namespace, key, value } => {
                ShardChange::Put { key: Key::new(&namespace, key), value: StoredValue::Inline(InlineValue(value)), expires_at: None }
            }
            Self::Delete { namespace, key } => ShardChange::Delete { key: Key::new(&namespace, key) },
        }
    }
}

/// Owns one data directory for the lifetime of a server process.
pub struct ServerStore {
    dir: PathBuf,
    catalog: PathBuf,
    shard_id: String,
    shard_path: PathBuf,
    lock: Option<PathBuf>,
    shard: FileShard,
}

impl ServerStore {
    /// Opens or creates the store under `dir` and takes the directory lock.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(io_error)?;
        let lock = take_lock(&dir)?;

        let catalog = dir.join(CATALOG_FILE);
        let legacy = dir.join(LEGACY_CATALOG_FILE);
        if legacy.exists() {
            drop_lock(&lock);
            return Err(protocol("legacy PostgreSQL catalog layout: delete the data directory and reinitialize"));
        }
        let shard_id = match catalog_shard_id(&catalog) {
            Ok(shard_id) => shard_id,
            Err(error) => {
                drop_lock(&lock);
                return Err(error);
            }
        };
        if !catalog.exists() {
            let bytes = encode_catalog_bytes(&shard_id)?;
            write_atomic(&catalog, &bytes)?;
        }
        let shard_path = dir.join(format!("{shard_id}.yykv"));
        let shard = match FileShard::open(&shard_path) {
            Ok(shard) => shard,
            Err(error) => {
                drop_lock(&lock);
                return Err(storage_error(error));
            }
        };
        Ok(Self { dir, catalog, shard_id, shard_path, lock: Some(lock), shard })
    }

    /// Absolute data directory.
    pub fn data_dir(&self) -> String {
        self.dir.display().to_string()
    }

    /// File stem of the single shard this store's catalog registers.
    pub fn shard_id(&self) -> &str {
        &self.shard_id
    }

    /// Absolute `.yykv` shard path.
    pub fn shard_path(&self) -> String {
        self.shard_path.display().to_string()
    }

    /// Absolute `.yyds` catalog path.
    pub fn catalog_path(&self) -> String {
        self.catalog.display().to_string()
    }

    /// Absolute `postmaster.pid` lock path.
    pub fn lock_path(&self) -> String {
        self.dir.join(LOCK_FILE).display().to_string()
    }

    /// Whether every [`Self::commit_changes`] call is durable across process exit.
    pub fn durable_atomic_commit(&self) -> bool {
        self.shard.durable_atomic_commit()
    }

    /// Ordered prefix scan inside one namespace.
    pub fn scan(
        &self,
        namespace: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<ServerRow>, Error> {
        let records = self.shard.scan(&Namespace(namespace.to_owned()), prefix, after, before, limit).map_err(storage_error)?;
        records
            .into_iter()
            .map(|record| {
                let StoredValue::Inline(value) = record.value
                else {
                    return Err(protocol("server store holds an object reference that is not inline"));
                };
                Ok(ServerRow { key: record.key.bytes, value: value.0 })
            })
            .collect::<Result<Vec<_>, Error>>()
    }

    /// Applies one statement's changes as a single durable atomic commit.
    pub fn commit_changes(&mut self, changes: Vec<ServerChange>) -> Result<(), Error> {
        let changes = changes.into_iter().map(ServerChange::into_shard_change).collect();
        self.shard.commit_changes(changes).map_err(storage_error)
    }

    /// Releases the directory lock.
    pub fn close(mut self) -> Result<(), Error> {
        if let Some(lock) = self.lock.take() {
            drop_lock(&lock);
        }
        Ok(())
    }
}

impl Drop for ServerStore {
    fn drop(&mut self) {
        if let Some(lock) = self.lock.take() {
            drop_lock(&lock);
        }
    }
}

fn storage_error(error: ClusterError) -> Error {
    match error {
        ClusterError::Io(error) => io_error(error),
        other => protocol(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir() -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        std::env::temp_dir().join(format!("postgres-server-store-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn commits_one_batch_and_reopens_either_whole_or_not_at_all() {
        let dir = scratch_dir();
        let mut store = ServerStore::open(&dir).expect("open");
        assert!(store.durable_atomic_commit());
        store
            .commit_changes(vec![
                ServerChange::Put { namespace: "pg_data.10".into(), key: b"a".to_vec(), value: b"1".to_vec() },
                ServerChange::Put { namespace: "pg_data.10".into(), key: b"b".to_vec(), value: b"2".to_vec() },
            ])
            .expect("commit");
        let rows = store.scan("pg_data.10", b"", None, None, 0).expect("scan");
        assert_eq!(rows.len(), 2);
        drop(store);

        let store = ServerStore::open(&dir).expect("reopen");
        let rows = store.scan("pg_data.10", b"", None, None, 0).expect("scan");
        assert_eq!(rows[0].key, b"a");
        assert_eq!(rows[0].value, b"1");
        assert_eq!(rows[1].key, b"b");
        store.close().expect("close");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_open_is_rejected_while_the_lock_holds() {
        let dir = scratch_dir();
        let store = ServerStore::open(&dir).expect("open");
        let error = ServerStore::open(&dir).err().expect("second open must fail");
        assert!(error.to_string().contains("already exists"));
        drop(store);
        assert!(ServerStore::open(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
