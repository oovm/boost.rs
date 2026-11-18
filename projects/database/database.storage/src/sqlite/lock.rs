//! Cross-process single-writer lock for a file-backed SQLite database.
//!
//! The lock lives in a `<database>.lock` sidecar and is taken through the
//! operating system's advisory file lock (`flock` / `LockFileEx`): one writer
//! may hold it at a time, regardless of the process that holds it. The OS
//! releases the lock when the holder exits, so a crashed writer never leaves a
//! stale lock behind. The PID stored inside the sidecar is diagnostic only and
//! never decides liveness.

use std::{
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A held writer lock. Dropping the value releases the OS lock.
pub struct WriterLock {
    /// File handle that owns the OS lock; drop releases it.
    _file: File,
}

/// Why a writer lock could not be acquired.
#[derive(Debug)]
pub enum LockError {
    /// The sidecar could not be opened.
    Open {
        /// Sidecar path that failed to open.
        path: PathBuf,
        /// Underlying filesystem failure.
        error: std::io::Error,
    },
    /// Another holder keeps the database locked past the configured timeout.
    Busy {
        /// Sidecar path carrying the recorded owner.
        path: PathBuf,
        /// PID the previous holder recorded, when readable.
        holder: Option<u32>,
    },
    /// The operating system rejected the lock request.
    Lock {
        /// Database file the lock belongs to.
        database: PathBuf,
        /// Underlying filesystem failure.
        error: std::io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, error } => write!(formatter, "cannot open lock file `{}`: {error}", path.display()),
            Self::Busy { holder, .. } => match holder {
                Some(pid) => write!(formatter, "database is locked by process {pid} (SQLITE_BUSY)"),
                None => formatter.write_str("database is locked (SQLITE_BUSY)"),
            },
            Self::Lock { database, error } => write!(formatter, "cannot lock database `{}`: {error}", database.display()),
        }
    }
}

impl std::error::Error for LockError {}

impl WriterLock {
    /// Sidecar path carrying the lock for `database`.
    pub fn sidecar(database: &Path) -> PathBuf {
        let mut path = database.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Acquire the write lock, retrying while another holder keeps it.
    ///
    /// `timeout` bounds the wait; `Duration::ZERO` makes exactly one attempt.
    pub fn acquire(database: &Path, timeout: Duration) -> Result<Self, LockError> {
        let path = Self::sidecar(database);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| LockError::Open { path: path.clone(), error })?;
        let deadline = Instant::now() + timeout;
        let mut wait = Duration::from_millis(1);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    record_owner(&file);
                    return Ok(Self { _file: file });
                }
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        let holder = recorded_owner(&path);
                        return Err(LockError::Busy { path, holder });
                    }
                    std::thread::sleep(wait.min(deadline - now));
                    wait = (wait * 2).min(Duration::from_millis(20));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(LockError::Lock { database: database.to_path_buf(), error });
                }
            }
        }
    }
}

/// Records the holder PID for diagnostics; a failure never changes the result.
fn record_owner(file: &File) {
    let mut handle = file;
    if handle.seek(SeekFrom::Start(0)).is_err() || file.set_len(0).is_err() {
        return;
    }
    let _ = handle.write_all(format!("{}\n", std::process::id()).as_bytes());
    let _ = handle.flush();
}

/// PID the sidecar records, when it holds a readable one.
fn recorded_owner(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok())
}
