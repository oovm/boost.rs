//! PostgreSQL data-directory storage failures.
//!
//! Disk, catalog and lease failures are storage facts and stay in this module.
//! SQLSTATE and wire wording are produced at the execution / server / client
//! boundary by mapping these variants; storage never depends on a wire error
//! type to describe a local commit failure.

use std::{fmt, path::PathBuf};

/// Failure raised while owning a PostgreSQL data directory.
#[derive(Debug)]
pub enum Error {
    /// Host filesystem failure: create, read, write, sync, replace or remove.
    Io(String),
    /// Catalog or shard bytes do not match the storage contract.
    Corrupt(String),
    /// Storage layout or option this product refuses explicitly.
    Unsupported(String),
    /// Another live process owns the `postmaster.pid` lease.
    Locked {
        /// Absolute lock path that already exists.
        path: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::Corrupt(message) | Self::Unsupported(message) => formatter.write_str(message),
            Self::Locked { path } => {
                write!(formatter, "lock file \"{}\" already exists: another server owns this data directory", path.display())
            }
        }
    }
}

impl std::error::Error for Error {}
