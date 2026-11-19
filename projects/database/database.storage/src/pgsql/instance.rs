//! PostgreSQL data-directory instance lease and table-group shard layout.
//!
//! One instance owns the directory layout the wire server and the bindings
//! share: the primary store (`local-0.yyds`, `local-0.yykv`, `postmaster.pid`)
//! plus one self-contained single-shard directory per table group under
//! `shards/<shard-id>/`.
//!
//! The policy lives here instead of in a facade or a binding because it is
//! observable PostgreSQL behaviour, not a transport detail:
//!
//! * a `postmaster.pid` whose recorded process is gone is reclaimed before the
//!   directory is opened, while a live holder keeps the lock owned,
//! * a shard id becomes one path segment, so it is restricted to the lowercase
//!   alphanumeric range a directory name can hold,
//! * table-group directories hold exactly the files this module writes, so the
//!   orphan sweep never removes anything it did not create.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use super::{
    Error,
    catalog::LOCK_FILE,
    store::{ServerChange, ServerRow, ServerStore},
};

/// Directory under the data directory holding one directory per table group.
pub const SHARD_ROOT: &str = "shards";

/// File inside the data directory holding the persisted SQL schema registry.
///
/// DDL applied by the gateway is written here, so a restart reopens the same
/// tables instead of asking the caller to replay its DDL.
pub const SCHEMA_FILE: &str = "schema.json";

/// File names one table-group directory can ever hold, as written by
/// [`ServerStore::open`] with the default catalog registration. Orphan cleanup
/// deletes exactly these and nothing else, so an unexpected file keeps its
/// directory in place instead of vanishing.
pub const SHARD_DIRECTORY_FILES: [&str; 5] =
    ["local-0.yyds", "local-0.yykv", "postmaster.pid", "local-0.yyds.tmp", "local-0.yykv.tmp"];

/// Whether a process with this id is still running, the way PostgreSQL checks
/// `postmaster.pid` before refusing a data directory.
///
/// Windows opens the process with `PROCESS_QUERY_LIMITED_INFORMATION` and
/// treats `STILL_ACTIVE` as alive; Unix sends signal `0`, where success or
/// `EPERM` both mean the process exists. PID reuse is accepted exactly as
/// PostgreSQL accepts it: a recycled id keeps the lock owned.
#[cfg(windows)]
pub fn process_exists(pid: u32) -> bool {
    use std::ffi::c_void;

    type Dword = u32;
    type Handle = *mut c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: Dword = 0x1000;
    const STILL_ACTIVE: Dword = 259;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: Dword, inherit_handle: i32, process_id: Dword) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut Dword) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: Dword = 0;
        let queried = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        queried && code == STILL_ACTIVE
    }
}

/// Unix branch of [`process_exists`]: signal `0` probes without delivering.
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    const EPERM: i32 = 1;

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn __error() -> *mut i32;
    }
    #[cfg(not(target_os = "macos"))]
    unsafe extern "C" {
        fn __errno_location() -> *mut i32;
    }

    let Ok(pid) = i32::try_from(pid)
    else {
        return false;
    };
    unsafe {
        if kill(pid, 0) == 0 {
            return true;
        }
        #[cfg(target_os = "macos")]
        let errno_ptr = __error();
        #[cfg(not(target_os = "macos"))]
        let errno_ptr = __errno_location();
        *errno_ptr == EPERM
    }
}

/// Platforms without a process probe cannot tell a stale lock from a live one,
/// so the lock is kept and the open fails as before.
#[cfg(not(any(windows, unix)))]
pub fn process_exists(_pid: u32) -> bool {
    true
}

/// Drops a `postmaster.pid` whose recorded process is gone, so the next
/// [`ServerStore::open`] takes the directory instead of asking the operator to
/// delete the file. A live holder, an unreadable file or an unparseable first
/// line all leave the lock in place and let the open fail as before.
pub fn clear_stale_lock(data_dir: impl AsRef<Path>) {
    let lock = data_dir.as_ref().join(LOCK_FILE);
    let Ok(bytes) = fs::read(&lock)
    else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let Some(first) = text.lines().next()
    else {
        return;
    };
    let Ok(pid) = first.trim().parse::<u32>()
    else {
        return;
    };
    if process_exists(pid) {
        return;
    }
    let _ = fs::remove_file(&lock);
}

/// A shard id becomes a directory name under [`SHARD_ROOT`], so it stays inside
/// the lowercase alphanumeric range a single path segment can hold: no
/// separators, no `..`, no NUL, nothing that could escape the shard root.
pub fn validate_shard_id(shard_id: &str) -> Result<(), Error> {
    let usable = shard_id.starts_with("local-")
        && shard_id.len() <= 128
        && shard_id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if usable {
        return Ok(());
    }
    Err(Error::Corrupt(format!("shard id {shard_id:?} is unusable as a directory name")))
}

/// Absolute table-group directory for `shard_id`.
pub fn shard_directory(data_dir: impl AsRef<Path>, shard_id: &str) -> PathBuf {
    data_dir.as_ref().join(SHARD_ROOT).join(shard_id)
}

/// One data directory: the primary store plus every table-group store opened so
/// far, kept warm so committing one group re-encodes only that group's bytes.
///
/// Every lock an instance took is released by [`ServerInstance::close`] or by
/// the [`ServerStore`] destructors when the instance is dropped.
pub struct ServerInstance {
    primary: ServerStore,
    shards: BTreeMap<String, ServerStore>,
}

impl ServerInstance {
    /// Opens or creates the data directory and takes its lock, reclaiming a
    /// `postmaster.pid` whose holder process is gone.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let data_dir = data_dir.as_ref();
        clear_stale_lock(data_dir);
        Ok(Self { primary: ServerStore::open(data_dir)?, shards: BTreeMap::new() })
    }

    /// Absolute data directory of the primary store.
    pub fn data_dir(&self) -> String {
        self.primary.data_dir()
    }

    /// Absolute `.yykv` shard path of the primary store.
    pub fn shard_path(&self) -> String {
        self.primary.shard_path()
    }

    /// File stem of the single shard the primary catalog registers.
    pub fn shard_id(&self) -> &str {
        self.primary.shard_id()
    }

    /// Absolute `.yyds` catalog path of the primary store.
    pub fn catalog_path(&self) -> String {
        self.primary.catalog_path()
    }

    /// Absolute path of the persisted SQL schema registry inside the primary
    /// data directory.
    pub fn schema_path(&self) -> String {
        Path::new(&self.primary.data_dir()).join(SCHEMA_FILE).to_string_lossy().into_owned()
    }

    /// Absolute `postmaster.pid` lock path of the primary store.
    pub fn lock_path(&self) -> String {
        self.primary.lock_path()
    }

    /// Whether every commit survives process exit, on every open store.
    pub fn durable_atomic_commit(&self) -> bool {
        self.primary.durable_atomic_commit() && self.shards.values().all(ServerStore::durable_atomic_commit)
    }

    /// Ordered prefix scan inside one namespace of the primary store.
    pub fn scan(
        &self,
        namespace: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<ServerRow>, Error> {
        self.primary.scan(namespace, prefix, after, before, limit)
    }

    /// Applies one statement's changes to the primary store as a single durable
    /// atomic commit.
    pub fn commit_changes(&mut self, changes: Vec<ServerChange>) -> Result<(), Error> {
        self.primary.commit_changes(changes)
    }

    /// Opens `shards/<shard-id>/` as a table-group store and keeps it warm. The
    /// directory is created when it does not exist yet.
    pub fn open_shard(&mut self, shard_id: &str) -> Result<(), Error> {
        self.shard(shard_id).map(|_| ())
    }

    /// Releases one shard's directory lock, keeping its files.
    pub fn close_shard(&mut self, shard_id: &str) -> Result<(), Error> {
        validate_shard_id(shard_id)?;
        match self.shards.remove(shard_id) {
            Some(store) => store.close(),
            None => Ok(()),
        }
    }

    /// Closes one shard and removes its directory.
    ///
    /// Only [`SHARD_DIRECTORY_FILES`] are deleted and the directory is dropped
    /// when it is left empty; a directory holding anything else keeps its
    /// contents in place.
    pub fn drop_shard(&mut self, shard_id: &str) -> Result<(), Error> {
        validate_shard_id(shard_id)?;
        let directory = shard_directory(self.primary.data_dir(), shard_id);
        if let Some(store) = self.shards.remove(shard_id) {
            store.close()?;
        }
        for name in SHARD_DIRECTORY_FILES {
            let _ = fs::remove_file(directory.join(name));
        }
        match fs::remove_dir(&directory) {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty) => Ok(()),
            Err(error) => Err(Error::Io(format!("cannot remove {}: {error}", directory.display()))),
        }
    }

    /// Shard ids whose directory exists under [`SHARD_ROOT`], sorted.
    pub fn list_shards(&self) -> Result<Vec<String>, Error> {
        let root = Path::new(&self.primary.data_dir()).join(SHARD_ROOT);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::Io(format!("{}: {error}", root.display()))),
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned)
            else {
                continue;
            };
            if validate_shard_id(&name).is_ok() {
                ids.push(name);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Whether `shards/<shard-id>/` exists. A missing shard directory reads as
    /// an empty group, so scans never create files on disk.
    pub fn shard_exists(&self, shard_id: &str) -> Result<bool, Error> {
        validate_shard_id(shard_id)?;
        Ok(shard_directory(self.primary.data_dir(), shard_id).exists())
    }

    /// Absolute shard directory, the single source of truth for the layout.
    pub fn shard_dir(&self, shard_id: &str) -> Result<String, Error> {
        validate_shard_id(shard_id)?;
        Ok(shard_directory(self.primary.data_dir(), shard_id).display().to_string())
    }

    /// Ordered prefix scan inside one table-group shard. A directory that does
    /// not exist reads as zero rows instead of being created, so a read of a
    /// group that was never written leaves no artifacts behind.
    pub fn scan_shard(
        &mut self,
        shard_id: &str,
        namespace: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<ServerRow>, Error> {
        validate_shard_id(shard_id)?;
        if !self.shards.contains_key(shard_id) && !shard_directory(self.primary.data_dir(), shard_id).exists() {
            return Ok(Vec::new());
        }
        self.shard(shard_id)?.scan(namespace, prefix, after, before, limit)
    }

    /// Applies one batch as a single durable atomic commit inside one
    /// table-group shard, opening the shard on demand.
    pub fn commit_shard(&mut self, shard_id: &str, changes: Vec<ServerChange>) -> Result<(), Error> {
        validate_shard_id(shard_id)?;
        self.shard(shard_id)?.commit_changes(changes)
    }

    /// Releases every directory lock, the table-group shards included.
    pub fn close(mut self) -> Result<(), Error> {
        let mut failure = None;
        for (_, store) in std::mem::take(&mut self.shards) {
            if let Err(error) = store.close() {
                failure.get_or_insert(error);
            }
        }
        if let Err(error) = self.primary.close() {
            failure.get_or_insert(error);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn shard(&mut self, shard_id: &str) -> Result<&mut ServerStore, Error> {
        validate_shard_id(shard_id)?;
        if !self.shards.contains_key(shard_id) {
            let directory = shard_directory(self.primary.data_dir(), shard_id);
            clear_stale_lock(&directory);
            let store = ServerStore::open(&directory)?;
            self.shards.insert(shard_id.to_owned(), store);
        }
        Ok(self.shards.get_mut(shard_id).expect("inserted above"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        std::env::temp_dir().join(format!("postgres-instance-{name}-{}-{nanos}", std::process::id()))
    }

    fn put(namespace: &str, key: &[u8], value: &[u8]) -> ServerChange {
        ServerChange::Put { namespace: namespace.to_owned(), key: key.to_vec(), value: value.to_vec() }
    }

    #[test]
    fn current_process_exists_and_a_bogus_pid_does_not() {
        assert!(process_exists(std::process::id()));
        // The probe must answer, not guess: an id no process can hold is dead.
        assert!(!process_exists(u32::MAX));
    }

    #[test]
    fn a_stale_lock_is_reclaimed_and_a_live_one_is_kept() {
        let dir = scratch_dir("stale");
        fs::create_dir_all(&dir).unwrap();
        let lock = dir.join(LOCK_FILE);

        fs::write(&lock, b"4294967295\n").unwrap();
        clear_stale_lock(&dir);
        assert!(!lock.exists());

        fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
        clear_stale_lock(&dir);
        assert!(lock.exists());

        fs::write(&lock, b"not-a-pid\n").unwrap();
        clear_stale_lock(&dir);
        assert!(lock.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_instance_opens_its_primary_store_and_reclaims_a_stale_lock() {
        let dir = scratch_dir("open");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(LOCK_FILE), b"4294967295\n").unwrap();

        let instance = ServerInstance::open(&dir).expect("stale lock is reclaimed");
        assert!(Path::new(&instance.lock_path()).exists());
        assert!(instance.durable_atomic_commit());
        instance.close().expect("close");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shard_ids_stay_one_path_segment() {
        assert!(validate_shard_id("local-0").is_ok());
        assert!(validate_shard_id("local-1a").is_ok());
        for unusable in ["", "local-0/../x", "local_0", "Local-0", "local-0\\x", "shadow"] {
            assert!(validate_shard_id(unusable).is_err(), "{unusable:?} must be rejected");
        }
    }

    #[test]
    fn table_group_shards_commit_scan_and_list() {
        let dir = scratch_dir("shards");
        let mut instance = ServerInstance::open(&dir).expect("open");
        instance.commit_changes(vec![put("pg_data.10", b"a", b"1")]).expect("primary commit");

        assert!(!instance.shard_exists("local-1").expect("exists"));
        assert!(instance.scan_shard("local-1", "pg_data.10", b"", None, None, 0).expect("empty scan").is_empty());

        instance.commit_shard("local-1", vec![put("pg_data.10", b"b", b"2")]).expect("shard commit");
        assert!(instance.shard_exists("local-1").expect("exists"));
        let rows = instance.scan_shard("local-1", "pg_data.10", b"", None, None, 0).expect("scan");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, b"2");
        assert_eq!(instance.list_shards().expect("list"), vec!["local-1".to_owned()]);
        assert!(instance.shard_dir("local-1").expect("dir").ends_with("local-1"));

        instance.close_shard("local-1").expect("close shard");
        assert!(instance.shard_exists("local-1").expect("exists"));

        instance.drop_shard("local-1").expect("drop shard");
        assert!(!instance.shard_exists("local-1").expect("exists"));
        assert!(instance.list_shards().expect("list").is_empty());

        let primary = instance.scan("pg_data.10", b"", None, None, 0).expect("primary scan");
        assert_eq!(primary.len(), 1);
        instance.close().expect("close");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_a_shard_keeps_unexpected_files() {
        let dir = scratch_dir("orphan");
        let mut instance = ServerInstance::open(&dir).expect("open");
        instance.open_shard("local-2").expect("open shard");
        let shard_dir = shard_directory(&dir, "local-2");
        fs::write(shard_dir.join("keep.bin"), b"x").unwrap();

        instance.drop_shard("local-2").expect("drop");
        assert!(shard_dir.exists());
        assert!(shard_dir.join("keep.bin").exists());
        instance.close().expect("close");
        let _ = fs::remove_dir_all(&dir);
    }
}
