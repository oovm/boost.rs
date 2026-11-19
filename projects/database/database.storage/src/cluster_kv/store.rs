use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
    ops::Bound,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use database_formats::yy::yykv::{DeltaOp, KV_MAGIC, Key, Record, StoredValue, decode_shard_image, encode_delta_frame, encode_snapshot};
use database_types::cluster::{Error, Namespace, Result, SweepReport};

/// Smallest delta area kept before a commit compacts the shard back into one base snapshot.
const COMPACT_MIN_DELTA_BYTES: usize = 8 * 1024 * 1024;
/// Upper bound of the compaction threshold, so a large shard still reclaims space regularly.
const COMPACT_MAX_DELTA_BYTES: usize = 256 * 1024 * 1024;

/// Current wall clock as Unix epoch milliseconds, used for record expiry deadlines.
pub fn now_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_millis() as i64).unwrap_or(0)
}

/// One mutation inside a shard-level atomic commit.
#[derive(Debug, Clone)]
pub enum ShardChange {
    /// Insert or replace a key with `value` at the next shard revision.
    Put {
        /// The key to insert or replace.
        key: Key,
        /// The post-commit value.
        value: StoredValue,
        /// Optional wall-clock expiry as Unix epoch milliseconds; `None` clears any previous expiry.
        expires_at: Option<i64>,
    },
    /// Remove a key when present.
    Delete {
        /// The key to remove.
        key: Key,
    },
}

/// Shard-local KV surface. Implementations back one `.yykv` shard.
pub trait KvStore {
    /// Returns the latest record for `key`, if present and not expired.
    fn get(&self, key: &Key) -> Result<Option<Record>>;

    /// Inserts or replaces `value` and returns the new revision. Any previous expiry is cleared.
    fn put(&mut self, key: Key, value: StoredValue) -> Result<u64>;

    /// Inserts or replaces `value` with a wall-clock expiry in Unix epoch milliseconds.
    fn put_expiring(&mut self, key: Key, value: StoredValue, expires_at: i64) -> Result<u64>;

    /// Deletes `key` when present. Returns whether a record was removed.
    fn delete(&mut self, key: &Key) -> Result<bool>;

    /// Applies the whole change set as one atomic commit step.
    ///
    /// The contract is a single commit boundary: the changes are staged first (the complete
    /// post-commit state is produced and written durably), then applied atomically, and only
    /// then published to readers. Either every change in `changes` is committed and durable
    /// when this returns `Ok`, or nothing is when it returns `Err`. A sequential fold over
    /// [`Self::put`] / [`Self::delete`] is not an acceptable implementation because a crash
    /// between two persists leaves a half-committed batch on disk.
    ///
    /// Stores without a durable atomic commit boundary must return `Error::Unsupported`
    /// (the default) instead of degrading to per-change execution.
    fn commit_changes(&mut self, changes: Vec<ShardChange>) -> Result<()> {
        let _ = changes;
        Err(Error::Unsupported("store does not provide a durable atomic commit boundary"))
    }

    /// Whether [`Self::commit_changes`] survives process exit.
    ///
    /// Executors may only open transactions against stores that answer `true`; a store whose
    /// commit boundary is process-volatile (e.g. an in-memory shard) must keep the default
    /// `false` so transactions are rejected instead of being disguised by an executor-side
    /// mutex.
    fn durable_atomic_commit(&self) -> bool {
        false
    }

    /// Ordered prefix scan within one namespace.
    ///
    /// Returns up to `limit` records whose key bytes start with `prefix` and compare greater
    /// exclusive lower cursor; `before` is the exclusive upper cursor. `limit` of `0`
    /// means no cap. Results are sorted by key bytes ascending.
    fn scan(&self, namespace: &Namespace, prefix: &[u8], after: Option<&[u8]>, before: Option<&[u8]>, limit: usize) -> Result<Vec<Record>>;

    /// Physically removes records whose expiry deadline has elapsed at `now_unix_ms`.
    ///
    /// `budget` caps how many records this pass **inspects**: `Some(n)` stops after `n`
    /// records and leaves a resumable key cursor, `None` scans the whole shard. The cursor is
    /// process-local and never part of the snapshot, so a reopen starts from the shard head.
    ///
    /// Reclaim only drops records that every read path already treats as absent, so it never
    /// changes observable data. When nothing has expired the store must not touch the disk.
    /// File-backed stores persist the removal through the same durable atomic commit
    /// boundary as foreground writes: a failure leaves the previous snapshot intact and
    /// visible. Stores without a reclamation contract keep the default
    /// `Error::Unsupported`.
    fn sweep_expired(&mut self, now_unix_ms: i64, budget: Option<usize>) -> Result<SweepReport> {
        let _ = (now_unix_ms, budget);
        Err(Error::Unsupported("store does not provide expiry reclamation"))
    }

    /// Namespaces that hold at least one record, sorted ascending.
    ///
    /// A shard can host several namespaces; hosts enumerate them before
    /// reporting sizes or flushing one boundary. Stores without enumeration
    /// keep the default `Error::Unsupported`.
    fn namespaces(&self) -> Result<Vec<Namespace>> {
        Err(Error::Unsupported("store does not enumerate namespaces"))
    }

    /// Number of live records inside `namespace`.
    ///
    /// Expired records count as absent, matching [`Self::get`] and [`Self::scan`].
    fn len(&self, namespace: &Namespace) -> Result<usize> {
        let _ = namespace;
        Err(Error::Unsupported("store does not report namespace length"))
    }

    /// Removes every record inside `namespace` and returns how many were removed.
    ///
    /// Revisions continue from the shard counter so a later write never reuses
    /// a flushed revision. File-backed stores persist through the same durable
    /// boundary as [`Self::commit_changes`].
    fn flush(&mut self, namespace: &Namespace) -> Result<usize> {
        let _ = namespace;
        Err(Error::Unsupported("store does not flush namespaces"))
    }
}

/// In-memory shard used by unit tests and early integrations.
#[derive(Debug)]
pub struct MemoryShard {
    records: BTreeMap<Key, Record>,
    /// Next revision to allocate; an empty shard starts at `1`.
    next_revision: u64,
    /// Resume point for a budgeted expiry pass; process-local and not observable.
    sweep_cursor: Option<Key>,
}

impl Default for MemoryShard {
    fn default() -> Self {
        Self { records: BTreeMap::new(), next_revision: 1, sweep_cursor: None }
    }
}

impl MemoryShard {
    /// Creates an empty shard.
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, key: Key, value: StoredValue, expires_at: Option<i64>) -> Result<u64> {
        let revision = self.next_revision;
        self.next_revision = revision.checked_add(1).ok_or(Error::Corrupt("shard revision exhausted"))?;
        let record = Record { key: key.clone(), value, revision, expires_at };
        self.records.insert(key, record);
        Ok(revision)
    }
}

impl KvStore for MemoryShard {
    fn get(&self, key: &Key) -> Result<Option<Record>> {
        let now = now_unix_ms();
        Ok(self.records.get(key).cloned().filter(|record| !record.is_expired_at(now)))
    }

    fn put(&mut self, key: Key, value: StoredValue) -> Result<u64> {
        self.insert(key, value, None)
    }

    fn put_expiring(&mut self, key: Key, value: StoredValue, expires_at: i64) -> Result<u64> {
        self.insert(key, value, Some(expires_at))
    }

    fn delete(&mut self, key: &Key) -> Result<bool> {
        Ok(self.records.remove(key).is_some())
    }

    fn scan(&self, namespace: &Namespace, prefix: &[u8], after: Option<&[u8]>, before: Option<&[u8]>, limit: usize) -> Result<Vec<Record>> {
        Ok(collect_scan(&self.records, namespace, prefix, after, before, limit))
    }

    fn sweep_expired(&mut self, now_unix_ms: i64, budget: Option<usize>) -> Result<SweepReport> {
        let (expired, examined, next_cursor, capped) = collect_expired(&self.records, now_unix_ms, budget, self.sweep_cursor.as_ref());
        self.sweep_cursor = if capped { next_cursor.or_else(|| self.sweep_cursor.clone()) } else { None };
        let reclaimed = expired.len();
        for key in expired {
            self.records.remove(&key);
        }
        Ok(SweepReport { examined, reclaimed })
    }

    fn namespaces(&self) -> Result<Vec<Namespace>> {
        Ok(namespaces_of(&self.records))
    }

    fn len(&self, namespace: &Namespace) -> Result<usize> {
        let now = now_unix_ms();
        Ok(self.records.values().filter(|record| record.key.namespace == *namespace && !record.is_expired_at(now)).count())
    }

    fn flush(&mut self, namespace: &Namespace) -> Result<usize> {
        let keys: Vec<Key> = self.records.keys().filter(|key| key.namespace == *namespace).cloned().collect();
        for key in &keys {
            self.records.remove(key);
        }
        Ok(keys.len())
    }
}

/// One open write-batch scope.
///
/// Changes staged inside the scope are applied to memory (so later reads and later
/// statements in the same command see them) but are only made durable when the
/// outermost scope ends. If that durable write fails, `pre_images` restores the exact
/// pre-scope memory state and `start_revision` is restored with it.
#[derive(Debug)]
struct WriteBatch {
    start_revision: u64,
    pre_images: CommitJournal,
    /// Final state of every key touched in the scope; `None` means the key is deleted.
    staged: BTreeMap<Key, Option<Record>>,
    depth: usize,
    /// Thread that opened the scope. Writes from any other thread are settled
    /// durably first, so one host's deferred writes are never carried into
    /// another host's commit or rollback.
    owner: std::thread::ThreadId,
}

/// File-backed shard: one compacted base snapshot followed by append-only delta frames.
///
/// A commit appends exactly the bytes it changed and fsyncs them; it never rewrites the
/// whole file. When the delta area grows past its threshold the shard is compacted back
/// into a single base snapshot, which is also what reclaims the space held by superseded
/// versions of rewritten or deleted keys.
#[derive(Debug)]
pub struct FileShard {
    path: PathBuf,
    records: BTreeMap<Key, Record>,
    next_revision: u64,
    /// Byte length of the durable base section, file header included.
    base_len: usize,
    /// Byte length of the delta frames appended after the base section.
    delta_len: usize,
    /// Set when a failed append could not be truncated back to the last frame boundary.
    tail_unverified: bool,
    /// Resume point for a budgeted expiry pass; process-local and not persisted.
    sweep_cursor: Option<Key>,
    /// Open write-batch scope, if any.
    batch: Option<WriteBatch>,
}

impl FileShard {
    /// Opens an existing shard or creates an empty shard at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        open_shard(path.as_ref(), true)
    }

    /// Opens a shard that must already exist.
    ///
    /// Unlike [`Self::open`], a missing file is `Error::NotFound` instead of
    /// being created. Hosts that only read existing data use this so a typo in
    /// a shard path cannot silently install an empty shard.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        open_shard(path.as_ref(), false)
    }

    /// Returns the backing `.yykv` path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrites the shard as one compacted base snapshot.
    ///
    /// Commits only append delta frames; this is the explicit space-reclaim entry point.
    pub fn flush(&mut self) -> Result<()> {
        self.compact()
    }

    /// Bytes the delta area may grow to before a commit compacts the shard.
    fn compaction_threshold(&self) -> usize {
        (self.base_len / 4).clamp(COMPACT_MIN_DELTA_BYTES, COMPACT_MAX_DELTA_BYTES)
    }

    /// Publishes the whole shard as a fresh base snapshot and drops the delta frames.
    ///
    /// This is the only path that rewrites the file: it collapses superseded versions of
    /// rewritten and deleted keys, so the on-disk size tracks live data instead of history.
    fn compact(&mut self) -> Result<()> {
        let bytes = encode_snapshot(&self.records, self.next_revision)?;
        write_snapshot_durable(&self.path, &bytes)?;
        self.base_len = bytes.len();
        self.delta_len = 0;
        self.tail_unverified = false;
        Ok(())
    }

    /// Appends one delta frame carrying `next_revision` and every op in the commit.
    fn append_frame(&mut self, next_revision: u64, ops: &[DeltaOp]) -> Result<()> {
        let frame = encode_delta_frame(next_revision, ops)?;
        self.append_bytes(&frame)
    }

    /// Makes `frame` durable by appending it after the last committed frame.
    ///
    /// The fsync is the commit point: memory is only published after it returns `Ok`, so a
    /// crash or storage failure leaves the previous commit boundary intact. A failed append
    /// is truncated back to that boundary; if even the truncation fails, the shard refuses
    /// further writes until a reopen cuts the torn tail.
    fn append_bytes(&mut self, frame: &[u8]) -> Result<()> {
        if self.tail_unverified {
            return Err(Error::Corrupt("yykv shard tail is unverified: reopen the shard before writing"));
        }
        let offset = (self.base_len + self.delta_len) as u64;
        let mut file = OpenOptions::new().write(true).open(&self.path)?;
        let appended = (|| -> std::io::Result<()> {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(frame)?;
            file.sync_all()
        })();
        if let Err(error) = appended {
            if file.set_len(offset).is_err() {
                self.tail_unverified = true;
            }
            return Err(Error::Io(error));
        }
        self.delta_len += frame.len();
        if self.delta_len >= self.compaction_threshold() {
            // The commit is already durable, so a failed compaction must not fail the write.
            // The frames stay valid and the next commit retries the compaction.
            let _ = self.compact();
        }
        Ok(())
    }

    /// Applies one record to memory.
    fn apply_put(&mut self, record: Record) {
        self.records.insert(record.key.clone(), record);
    }

    /// Removes one key from memory.
    fn apply_delete(&mut self, key: &Key) {
        self.records.remove(key);
    }

    fn insert(&mut self, key: Key, value: StoredValue, expires_at: Option<i64>) -> Result<u64> {
        let revision = self.next_revision;
        let next_revision = revision.checked_add(1).ok_or(Error::Corrupt("shard revision exhausted"))?;
        let record = Record { key: key.clone(), value, revision, expires_at };
        let mut journal = CommitJournal::new();
        journal.capture(self, &key);
        self.apply_put(record.clone());
        if self.owns_batch() {
            self.stage(&key, Some(record));
            self.absorb(journal);
            self.next_revision = next_revision;
            return Ok(revision);
        }
        if self.batch.is_some() {
            self.stage(&key, Some(record));
            if let Err(error) = self.flush_staged(next_revision) {
                journal.restore(self);
                return Err(error);
            }
            self.next_revision = next_revision;
            return Ok(revision);
        }
        if let Err(error) = self.append_frame(next_revision, &[DeltaOp::Put(record)]) {
            journal.restore(self);
            return Err(error);
        }
        self.next_revision = next_revision;
        Ok(revision)
    }

    /// Whether the calling thread owns the open write-batch scope.
    fn owns_batch(&self) -> bool {
        self.batch.as_ref().is_some_and(|batch| batch.owner == std::thread::current().id())
    }

    /// Opens a write-batch scope.
    ///
    /// Inside the scope every write is applied to memory immediately (readers, including
    /// later statements of the same command, see it) but only the outermost
    /// [`Self::end_write_batch`] appends a delta frame. Scopes nest; the innermost `flush`
    /// of durability is the outermost end.
    pub fn begin_write_batch(&mut self) {
        match self.batch.as_mut() {
            Some(batch) => batch.depth += 1,
            None => {
                self.batch = Some(WriteBatch {
                    start_revision: self.next_revision,
                    pre_images: CommitJournal::new(),
                    staged: BTreeMap::new(),
                    depth: 1,
                    owner: std::thread::current().id(),
                });
            }
        }
    }

    /// Records the final state of one key inside the open scope.
    fn stage(&mut self, key: &Key, record: Option<Record>) {
        if let Some(batch) = self.batch.as_mut() {
            batch.staged.insert(key.clone(), record);
        }
    }

    /// Closes a write-batch scope and makes every staged write durable with one delta frame.
    ///
    /// A failed durable write restores the exact state from before the outermost scope, so a
    /// caller that reports the failure can never leave a half-committed command visible.
    pub fn end_write_batch(&mut self) -> Result<()> {
        let Some(batch) = self.batch.as_ref()
        else {
            return Ok(());
        };
        if batch.depth > 1 {
            if let Some(batch) = self.batch.as_mut() {
                batch.depth -= 1;
            }
            return Ok(());
        }
        let start_revision = batch.start_revision;
        // A scope that staged nothing must not append a frame.
        if batch.staged.is_empty() && self.next_revision == start_revision {
            self.batch = None;
            return Ok(());
        }
        let pre_images = std::mem::take(&mut self.batch.as_mut().expect("batch was just read").pre_images);
        let next_revision = self.next_revision;
        if let Err(error) = self.flush_staged(next_revision) {
            pre_images.restore(self);
            self.next_revision = start_revision;
            self.batch = None;
            return Err(error);
        }
        self.batch = None;
        Ok(())
    }

    /// Appends every staged key of the open scope as one frame and rebases the scope onto it.
    ///
    /// This also settles a scope owned by another thread: its staged state becomes durable
    /// together with the caller's write, and its pre-images are dropped so a later rollback
    /// can never move the disk state backwards.
    fn flush_staged(&mut self, next_revision: u64) -> Result<()> {
        let Some(batch) = self.batch.as_mut()
        else {
            return Ok(());
        };
        let staged = std::mem::take(&mut batch.staged);
        let ops: Vec<DeltaOp> = staged
            .iter()
            .map(|(key, record)| match record {
                Some(record) => DeltaOp::Put(record.clone()),
                None => DeltaOp::Delete(key.clone()),
            })
            .collect();
        if !ops.is_empty() {
            if let Err(error) = self.append_frame(next_revision, &ops) {
                if let Some(batch) = self.batch.as_mut() {
                    batch.staged = staged;
                }
                return Err(error);
            }
        }
        if let Some(batch) = self.batch.as_mut() {
            batch.pre_images = CommitJournal::new();
            batch.start_revision = next_revision;
        }
        Ok(())
    }

    /// Merges one commit's pre-images into the open scope's journal.
    ///
    /// The earliest pre-image of a key wins, so restoring the scope rewinds every key to the
    /// state it had when the scope opened.
    fn absorb(&mut self, journal: CommitJournal) {
        let Some(batch) = self.batch.as_mut()
        else {
            return;
        };
        for (key, record) in journal.entries {
            if batch.pre_images.seen.insert(key.clone()) {
                batch.pre_images.entries.push((key, record));
            }
        }
    }
}

/// Pre-images of the keys one commit touches, so a failed durable write restores the exact
/// pre-commit memory state instead of leaving a half-applied batch visible to readers.
#[derive(Debug, Default)]
struct CommitJournal {
    seen: BTreeSet<Key>,
    entries: Vec<(Key, Option<Record>)>,
}

impl CommitJournal {
    fn new() -> Self {
        Self { seen: BTreeSet::new(), entries: Vec::new() }
    }

    /// Records the pre-image of `key` once; repeated touches in the same batch are no-ops.
    fn capture(&mut self, shard: &FileShard, key: &Key) {
        if self.seen.insert(key.clone()) {
            self.entries.push((key.clone(), shard.records.get(key).cloned()));
        }
    }

    fn restore(self, shard: &mut FileShard) {
        for (key, record) in self.entries.into_iter().rev() {
            match record {
                Some(record) => {
                    shard.records.insert(key, record);
                }
                None => {
                    shard.records.remove(&key);
                }
            }
        }
    }
}

fn change_key(change: &ShardChange) -> &Key {
    match change {
        ShardChange::Put { key, .. } => key,
        ShardChange::Delete { key } => key,
    }
}

impl KvStore for FileShard {
    fn get(&self, key: &Key) -> Result<Option<Record>> {
        let now = now_unix_ms();
        Ok(self.records.get(key).cloned().filter(|record| !record.is_expired_at(now)))
    }

    fn put(&mut self, key: Key, value: StoredValue) -> Result<u64> {
        self.insert(key, value, None)
    }

    fn put_expiring(&mut self, key: Key, value: StoredValue, expires_at: i64) -> Result<u64> {
        self.insert(key, value, Some(expires_at))
    }

    fn delete(&mut self, key: &Key) -> Result<bool> {
        if !self.records.contains_key(key) {
            return Ok(false);
        }
        let mut journal = CommitJournal::new();
        journal.capture(self, key);
        self.apply_delete(key);
        if self.owns_batch() {
            self.stage(key, None);
            self.absorb(journal);
            return Ok(true);
        }
        if self.batch.is_some() {
            self.stage(key, None);
            if let Err(error) = self.flush_staged(self.next_revision) {
                journal.restore(self);
                return Err(error);
            }
            return Ok(true);
        }
        if let Err(error) = self.append_frame(self.next_revision, &[DeltaOp::Delete(key.clone())]) {
            journal.restore(self);
            return Err(error);
        }
        Ok(true)
    }

    fn commit_changes(&mut self, changes: Vec<ShardChange>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // The revision budget is checked before any memory is touched, so an exhausted
        // counter leaves the shard exactly as it was, matching the pre-staging contract.
        let puts = changes.iter().filter(|change| matches!(change, ShardChange::Put { .. })).count() as u64;
        let next_revision = self.next_revision.checked_add(puts).ok_or(Error::Corrupt("shard revision exhausted"))?;
        let mut journal = CommitJournal::new();
        for change in &changes {
            journal.capture(self, change_key(change));
        }
        // Stage into memory and collect the frame ops in one pass. A failure anywhere restores
        // the journaled pre-images, so the visible state is either the whole batch or nothing.
        let mut ops = Vec::with_capacity(changes.len());
        let mut revision = self.next_revision;
        for change in changes {
            match change {
                ShardChange::Put { key, value, expires_at } => {
                    let record = Record { key, value, revision, expires_at };
                    revision = revision.checked_add(1).ok_or(Error::Corrupt("shard revision exhausted"))?;
                    self.apply_put(record.clone());
                    ops.push(DeltaOp::Put(record));
                }
                ShardChange::Delete { key } => {
                    self.apply_delete(&key);
                    ops.push(DeltaOp::Delete(key));
                }
            }
        }
        let staged = self.batch.is_some();
        if staged {
            for op in &ops {
                match op {
                    DeltaOp::Put(record) => self.stage(&record.key, Some(record.clone())),
                    DeltaOp::Delete(key) => self.stage(key, None),
                }
            }
        }
        match self.batch.as_ref() {
            Some(batch) if batch.owner == std::thread::current().id() => {
                // Inside the owning thread's scope the change is only published to memory;
                // the scope owner makes it durable with one delta frame when the command
                // finishes.
                self.absorb(journal);
                self.next_revision = next_revision;
                return Ok(());
            }
            Some(_) => {
                // Another thread's scope is open. Its staged state and this change are
                // appended together right away, and the scope is rebased onto that frame, so
                // this write is neither deferred behind the other scope nor rolled back with
                // it.
                if let Err(error) = self.flush_staged(next_revision) {
                    journal.restore(self);
                    return Err(error);
                }
                self.next_revision = next_revision;
                return Ok(());
            }
            None => {}
        }
        if let Err(error) = self.append_frame(next_revision, &ops) {
            journal.restore(self);
            return Err(error);
        }
        self.next_revision = next_revision;
        Ok(())
    }

    fn durable_atomic_commit(&self) -> bool {
        true
    }

    fn scan(&self, namespace: &Namespace, prefix: &[u8], after: Option<&[u8]>, before: Option<&[u8]>, limit: usize) -> Result<Vec<Record>> {
        Ok(collect_scan(&self.records, namespace, prefix, after, before, limit))
    }

    fn sweep_expired(&mut self, now_unix_ms: i64, budget: Option<usize>) -> Result<SweepReport> {
        let (expired, examined, next_cursor, capped) = collect_expired(&self.records, now_unix_ms, budget, self.sweep_cursor.as_ref());
        // The cursor only advances after a successful commit, so a storage failure retries
        // the same batch instead of skipping records that are still on disk.
        if expired.is_empty() {
            self.sweep_cursor = if capped { next_cursor.or_else(|| self.sweep_cursor.clone()) } else { None };
            // No reclamation work: the snapshot on disk stays untouched.
            return Ok(SweepReport { examined, reclaimed: 0 });
        }
        let reclaimed = expired.len();
        let changes = expired.into_iter().map(|key| ShardChange::Delete { key }).collect();
        self.commit_changes(changes)?;
        self.sweep_cursor = if capped { next_cursor.or_else(|| self.sweep_cursor.clone()) } else { None };
        Ok(SweepReport { examined, reclaimed })
    }

    fn namespaces(&self) -> Result<Vec<Namespace>> {
        Ok(namespaces_of(&self.records))
    }

    fn len(&self, namespace: &Namespace) -> Result<usize> {
        let now = now_unix_ms();
        Ok(self.records.values().filter(|record| record.key.namespace == *namespace && !record.is_expired_at(now)).count())
    }

    fn flush(&mut self, namespace: &Namespace) -> Result<usize> {
        let changes: Vec<ShardChange> =
            self.records.keys().filter(|key| key.namespace == *namespace).map(|key| ShardChange::Delete { key: key.clone() }).collect();
        let removed = changes.len();
        if removed > 0 {
            self.commit_changes(changes)?;
        }
        Ok(removed)
    }
}

/// Distinct namespaces present in a shard, sorted ascending.
fn namespaces_of(records: &BTreeMap<Key, Record>) -> Vec<Namespace> {
    let mut namespaces: Vec<Namespace> = records.keys().map(|key| key.namespace.clone()).collect();
    namespaces.sort();
    namespaces.dedup();
    namespaces
}

/// Selects expired records for one budgeted pass.
///
/// Returns the expired keys, how many records were inspected, the new resume cursor and
/// whether the budget stopped the pass before the shard end.
fn collect_expired(
    records: &BTreeMap<Key, Record>,
    now_unix_ms: i64,
    budget: Option<usize>,
    cursor: Option<&Key>,
) -> (Vec<Key>, usize, Option<Key>, bool) {
    let mut expired = Vec::new();
    let mut examined = 0;
    let mut last_examined = None;
    let mut capped = false;
    let iterator = match cursor {
        Some(cursor) => records.range((Bound::Excluded(cursor), Bound::Unbounded)),
        None => records.range(..),
    };
    for (key, record) in iterator {
        if budget.is_some_and(|limit| examined >= limit) {
            capped = true;
            break;
        }
        examined += 1;
        last_examined = Some(key.clone());
        if record.is_expired_at(now_unix_ms) {
            expired.push(key.clone());
        }
    }
    (expired, examined, last_examined, capped)
}

/// Shared scan logic: filter by namespace, prefix-match, exclusive cursors (after/before), limit.
/// Expired records are treated as absent.
///
/// 键序是 `(namespace, bytes)`，同一命名空间里带同一前缀的键是连续区间，所以这里
/// 从区间起点直接 `range` 过去，只走真正可能命中的记录。逐个 `continue` 过滤会让
/// 每次前缀探针都退化成整 shard 遍历，索引回表与唯一性判定都会因此变慢。
fn collect_scan(
    records: &BTreeMap<Key, Record>,
    namespace: &Namespace,
    prefix: &[u8],
    after: Option<&[u8]>,
    before: Option<&[u8]>,
    limit: usize,
) -> Vec<Record> {
    let now = now_unix_ms();
    let mut out = Vec::new();
    let start = Key::new(namespace.0.clone(), prefix.to_vec());
    for (key, record) in records.range(start..) {
        if key.namespace != *namespace || !key.bytes.starts_with(prefix) {
            break;
        }
        if record.is_expired_at(now) {
            continue;
        }
        if let Some(after_bytes) = after {
            if key.bytes.as_slice() <= after_bytes {
                continue;
            }
        }
        if let Some(before_bytes) = before {
            if key.bytes.as_slice() >= before_bytes {
                continue;
            }
        }
        out.push(record.clone());
        if limit > 0 && out.len() >= limit {
            break;
        }
    }
    out
}

/// Publishes one shard image through the storage-wide atomic publish contract.
///
/// The commit point, replacement primitive and failure semantics are the ones
/// [`crate::local::publish`] proves for every database file: temp write, one
/// directory-entry replace, parent sync. The shard keeps its own temp name and
/// its own snapshot encoding, but not a second `rename` sequence.
fn write_snapshot_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp =
        path.with_file_name(format!(".{}.yykv-tmp-{}", path.file_name().and_then(|name| name.to_str()).unwrap_or("shard"), std::process::id()));
    crate::local::publish::publish_with_temp(path, bytes, temp).map(|_| ()).map_err(Error::from)
}

/// Opens a shard file, replaying its delta frames and normalizing it to the current layout.
fn open_shard(path: &Path, create_missing: bool) -> Result<FileShard> {
    if !path.exists() {
        if !create_missing {
            return Err(Error::NotFound { key: path.display().to_string() });
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut shard = FileShard {
            path: path.to_path_buf(),
            records: BTreeMap::new(),
            next_revision: 1,
            base_len: 0,
            delta_len: 0,
            tail_unverified: false,
            sweep_cursor: None,
            batch: None,
        };
        shard.compact()?;
        return Ok(shard);
    }
    let bytes = fs::read(path)?;
    let image = decode_shard_image(&bytes)?;
    let committed = image.base_len + image.delta_len;
    let mut shard = FileShard {
        path: path.to_path_buf(),
        records: image.records,
        next_revision: image.next_revision,
        base_len: image.base_len,
        delta_len: image.delta_len,
        tail_unverified: false,
        sweep_cursor: None,
        batch: None,
    };
    if committed != bytes.len() {
        // An interrupted append never crossed a commit boundary: cut the partial frame so the
        // next append starts on a frame boundary.
        OpenOptions::new().write(true).open(path)?.set_len(committed as u64)?;
    }
    if !bytes.starts_with(KV_MAGIC) {
        // A version 3 shard is rewritten once as a version 4 base so later commits append
        // frames instead of replacing the whole file.
        shard.compact()?;
    }
    Ok(shard)
}

/// Compare-and-set helper built on top of [`KvStore`].
pub fn compare_and_put(store: &mut dyn KvStore, key: Key, expect_revision: Option<u64>, value: StoredValue) -> Result<u64> {
    let current = store.get(&key)?;
    match (expect_revision, current) {
        (None, Some(_)) => Err(Error::CasConflict { key: key.display() }),
        (Some(expected), Some(record)) if record.revision != expected => Err(Error::CasConflict { key: key.display() }),
        _ => store.put(key, value),
    }
}
