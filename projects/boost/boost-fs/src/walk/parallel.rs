//! Parallel traversal that aggregates one folder tree into totals.
//!
//! [`Walker`] yields one entry at a time, which keeps a single-threaded consumer simple but makes
//! it wait for the host on every listing. `scan` walks the same tree with several workers and
//! returns only the totals, so sizing a large tree (a build directory, a package store) uses the
//! storage device's queue depth instead of one request at a time.
//!
//! The rules match the sequential walk: reparse points are never followed, an unreadable folder
//! is skipped and counted instead of ending the scan, and the root itself is always counted as a
//! folder even when its listing fails.

use std::{
    ffi::OsString,
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::SystemTime,
};

#[cfg(test)]
use super::Walker;

/// How many failing paths [`Scan`] keeps for the caller to report.
const REPORTED_ERRORS: usize = 8;

/// Upper bound on worker threads, so a machine with very wide parallelism does not open a
/// disproportionate number of directory handles for one tree.
const MAX_THREADS: usize = 32;

/// One immediate entry of the scanned root, captured by the same listing that counts it.
#[derive(Debug, Clone)]
pub struct TopEntry {
    /// File name inside the root, exactly as the host reports it.
    pub name: OsString,
    /// What the listing classified this entry as.
    pub kind: TopKind,
    /// Byte length for a file; `0` for a folder or a link.
    pub len: u64,
    /// Modification time as the listing reported it.
    pub modified: Option<SystemTime>,
}

/// Classification of a [`TopEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopKind {
    /// A regular file.
    File,
    /// A folder that is not a reparse point.
    Folder,
    /// A symbolic link or a reparse point. Never followed and never counted as bytes.
    Link,
}

/// Options for [`scan`].
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Deepest folder depth that is listed. The root sits at depth `0`, so `0` counts the root's
    /// own entries without descending into any of them.
    pub max_depth: usize,
    /// Worker threads. `0` asks for the host's available parallelism, clamped to 1..=32.
    pub threads: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { max_depth: usize::MAX, threads: 0 }
    }
}

/// Totals of one tree, aggregated across workers.
#[derive(Debug, Clone)]
pub struct Scan {
    /// Regular files counted, reparse points excluded.
    pub files: u64,
    /// Folders counted, including the root.
    pub folders: u64,
    /// Reparse points met; they are neither descended nor measured.
    pub links: u64,
    /// Summed byte length of the files counted.
    pub bytes: u64,
    /// Listings and entries that could not be read. Any value above zero makes every other
    /// total a lower bound.
    pub errors: u64,
    /// Up to eight paths whose listing failed, so a caller can name the gaps.
    pub error_paths: Vec<PathBuf>,
    /// Newest file modification time seen.
    pub newest_modified: Option<SystemTime>,
    /// Entries of the root folder, unsorted.
    pub top: Vec<TopEntry>,
    /// The root's own listing succeeded. When it did not, `top` is empty and the scan describes
    /// nothing.
    pub root_readable: bool,
    /// Worker threads that ran. `1` means the scan ran on the calling thread.
    pub threads: usize,
}

impl Scan {
    /// Bytes the tree would free as a plain sum of file lengths.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Size one tree with parallel folder listings.
///
/// The result is identical to a sequential walk that never follows links and skips unreadable
/// folders; only the totals are kept, so nothing is returned per entry except the root's own
/// children.
pub fn scan(root: impl AsRef<Path>, options: &ScanOptions) -> Scan {
    let root = root.as_ref();
    let max_depth = options.max_depth;
    let threads = worker_count(options.threads);
    let shared = Shared::new(max_depth);

    if threads == 1 {
        let mut stack = vec![Job { path: root.to_path_buf(), depth: 0 }];
        while let Some(job) = stack.pop() {
            stack.extend(shared.visit(job));
        }
        shared.finish(1)
    }
    else {
        // The root job is what the initial `outstanding` count stands for, so it must be queued
        // before any worker looks for work.
        shared.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(Job { path: root.to_path_buf(), depth: 0 });
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| shared.run());
            }
        });
        shared.finish(threads)
    }
}

/// Worker count for a request: `0` means "ask the host", and every answer is clamped.
fn worker_count(requested: usize) -> usize {
    if requested > 0 {
        return requested.min(MAX_THREADS);
    }
    std::thread::available_parallelism().map_or(1, |width| width.get()).clamp(1, MAX_THREADS)
}

/// One folder waiting to be listed.
struct Job {
    path: PathBuf,
    depth: usize,
}

/// State shared by every worker of one scan.
struct Shared {
    max_depth: usize,
    queue: Mutex<Vec<Job>>,
    wake: Condvar,
    /// Folders queued or being listed. The scan ends when it reaches zero.
    outstanding: AtomicUsize,
    files: AtomicU64,
    folders: AtomicU64,
    links: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    /// Newest file modification time in milliseconds since the Unix epoch; `0` means "none yet".
    newest_ms: AtomicU64,
    top: Mutex<Vec<TopEntry>>,
    error_paths: Mutex<Vec<PathBuf>>,
    root_readable: AtomicBool,
}

impl Shared {
    fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
            queue: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            outstanding: AtomicUsize::new(1),
            files: AtomicU64::new(0),
            folders: AtomicU64::new(0),
            links: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            newest_ms: AtomicU64::new(0),
            top: Mutex::new(Vec::new()),
            error_paths: Mutex::new(Vec::new()),
            root_readable: AtomicBool::new(true),
        }
    }

    /// Worker loop: list folders until no folder is queued or in flight.
    fn run(&self) {
        loop {
            let job = {
                let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                loop {
                    if let Some(job) = queue.pop() {
                        break job;
                    }
                    if self.outstanding.load(Ordering::Acquire) == 0 {
                        return;
                    }
                    queue = self.wake.wait(queue).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };

            let children = self.visit(job);

            let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            // A worker only waits on an empty queue, so that state is what decides whether the
            // new children need a wake-up. Notifying every folder would wake all workers to let
            // exactly one claim the job, and a deeply nested tree pays that cost per directory.
            let was_empty = queue.is_empty();
            let queued = !children.is_empty();
            if queued {
                self.outstanding.fetch_add(children.len(), Ordering::AcqRel);
                queue.extend(children);
            }
            // The folder just listed is finished. The last decrement closes the scan, and every
            // wait wakes to observe it.
            let closed = self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1;
            drop(queue);
            if closed || (was_empty && queued) {
                self.wake.notify_all();
            }
        }
    }

    /// List one folder, fold its entries into the totals, and return the subfolders to list.
    fn visit(&self, job: Job) -> Vec<Job> {
        let read = match fs::read_dir(&job.path) {
            Ok(read) => read,
            Err(_) => {
                self.record_error(&job.path);
                if job.depth == 0 {
                    self.root_readable.store(false, Ordering::Release);
                }
                return Vec::new();
            }
        };

        let capture_top = job.depth == 0;
        let mut children = Vec::new();
        let mut top = Vec::new();

        for item in read {
            let Ok(entry) = item
            else {
                self.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let Ok(file_type) = entry.file_type()
            else {
                self.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            // On Windows the listing already carries the attributes, so `metadata` is a struct
            // copy. On Unix it is the single `lstat` a size aggregation pays; a subfolder
            // contributes neither bytes nor a timestamp this scan uses, so only files and the
            // root's own children read it. An entry whose attributes are needed but cannot be
            // read cannot be measured, so it is counted as an error instead of contributing a
            // size of zero.
            #[cfg(windows)]
            let metadata = match entry.metadata() {
                Ok(metadata) => Some(metadata),
                Err(_) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            #[cfg(not(windows))]
            let metadata = if file_type.is_dir() && !capture_top {
                None
            }
            else {
                match entry.metadata() {
                    Ok(metadata) => Some(metadata),
                    Err(_) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            };
            let link = file_type.is_symlink() || metadata.as_ref().is_some_and(is_reparse);
            let name = entry.file_name();
            let len = metadata.as_ref().map_or(0, Metadata::len);
            let modified = metadata.as_ref().and_then(|metadata| metadata.modified().ok());

            if link {
                self.links.fetch_add(1, Ordering::Relaxed);
                if capture_top {
                    top.push(TopEntry { name, kind: TopKind::Link, len: 0, modified });
                }
                continue;
            }

            if file_type.is_dir() {
                self.folders.fetch_add(1, Ordering::Relaxed);
                if capture_top {
                    top.push(TopEntry { name, kind: TopKind::Folder, len: 0, modified });
                }
                let depth = job.depth + 1;
                if depth <= self.max_depth {
                    children.push(Job { path: entry.path(), depth });
                }
                continue;
            }

            if file_type.is_file() {
                self.files.fetch_add(1, Ordering::Relaxed);
                self.bytes.fetch_add(len, Ordering::Relaxed);
                if let Some(stamp) = modified.and_then(unix_millis) {
                    self.newest_ms.fetch_max(stamp, Ordering::Relaxed);
                }
                if capture_top {
                    top.push(TopEntry { name, kind: TopKind::File, len, modified });
                }
            }
        }

        if capture_top {
            let mut sink = self.top.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sink.extend(top);
        }
        children
    }

    fn record_error(&self, path: &Path) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut paths = self.error_paths.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if paths.len() < REPORTED_ERRORS {
            paths.push(path.to_path_buf());
        }
    }

    fn finish(self, threads: usize) -> Scan {
        let root_readable = self.root_readable.load(Ordering::Acquire);
        let top = self.top.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        let error_paths = self.error_paths.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        let newest = match self.newest_ms.load(Ordering::Acquire) {
            0 => None,
            stamp => Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(stamp)),
        };
        Scan {
            files: self.files.load(Ordering::Acquire),
            // The root is a folder even when its listing failed.
            folders: self.folders.load(Ordering::Acquire) + 1,
            links: self.links.load(Ordering::Acquire),
            bytes: self.bytes.load(Ordering::Acquire),
            errors: self.errors.load(Ordering::Acquire),
            error_paths,
            newest_modified: if root_readable { newest } else { None },
            top,
            root_readable,
            threads,
        }
    }
}

fn unix_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH).ok().map(|elapsed| elapsed.as_millis() as u64)
}

/// Reparse points can form cycles on Windows, so a folder carrying one is never descended.
#[cfg(windows)]
fn is_reparse(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Outside Windows a folder listing never resolves a symbolic link, so the file type answered it.
#[cfg(not(windows))]
fn is_reparse(_meta: &Metadata) -> bool {
    false
}

/// The sequential walk that `scan` must agree with, used by the tests as the reference.
#[cfg(test)]
fn reference(root: &Path, max_depth: usize) -> Scan {
    let mut files = 0;
    let mut folders = 0;
    let mut links = 0;
    let mut bytes = 0;
    let mut newest: Option<SystemTime> = None;
    let mut top = Vec::new();
    for item in Walker::new(root).max_depth(max_depth) {
        let Ok(entry) = item
        else {
            continue;
        };
        let Ok(meta) = entry.metadata()
        else {
            continue;
        };
        if entry.is_folder() {
            folders += 1;
        }
        let link = entry.is_symlink() || is_reparse(&meta);
        if entry.depth() == 1 {
            let kind = if link {
                TopKind::Link
            }
            else if entry.is_folder() {
                TopKind::Folder
            }
            else {
                TopKind::File
            };
            top.push(TopEntry {
                name: entry.file_name().to_os_string(),
                kind,
                len: if kind == TopKind::File { meta.len() } else { 0 },
                modified: meta.modified().ok(),
            });
        }
        if link {
            links += 1;
            continue;
        }
        if entry.is_file() {
            files += 1;
            bytes += meta.len();
            if let Some(stamp) = meta.modified().ok().and_then(unix_millis) {
                newest = Some(newest.map_or(std::time::UNIX_EPOCH + std::time::Duration::from_millis(stamp), |current| {
                    current.max(std::time::UNIX_EPOCH + std::time::Duration::from_millis(stamp))
                }));
            }
        }
    }
    Scan { files, folders, links, bytes, errors: 0, error_paths: Vec::new(), newest_modified: newest, top, root_readable: true, threads: 1 }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, path::Path as StdPath};

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boost-fs-parallel-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &StdPath, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![b'x'; bytes]).unwrap();
    }

    fn sorted(mut entries: Vec<TopEntry>) -> Vec<(String, TopKind, u64)> {
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        entries.into_iter().map(|entry| (entry.name.to_string_lossy().to_string(), entry.kind, entry.len)).collect()
    }

    #[test]
    fn parallel_totals_match_the_sequential_walk() {
        let root = fixture("totals");
        for index in 0..12 {
            write(&root.join(format!("f{index:02}.bin")), 64 + index);
            write(&root.join(format!("nested{index}/deep/leaf.bin")), 8);
        }
        write(&root.join("nested0/empty-marker.txt"), 0);

        for threads in [1, 2, 4, 8] {
            let scan = scan(&root, &ScanOptions { max_depth: usize::MAX, threads });
            let expected = reference(&root, usize::MAX);
            assert_eq!(scan.files, expected.files, "threads={threads}");
            assert_eq!(scan.folders, expected.folders, "threads={threads}");
            assert_eq!(scan.bytes, expected.bytes, "threads={threads}");
            assert_eq!(scan.errors, 0, "threads={threads}");
            assert_eq!(sorted(scan.top), sorted(expected.top), "threads={threads}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn max_depth_bounds_the_listing() {
        let root = fixture("depth");
        write(&root.join("a/b/c.bin"), 16);
        write(&root.join("top.bin"), 4);

        let shallow = scan(&root, &ScanOptions { max_depth: 0, threads: 2 });
        assert_eq!(shallow.files, 1, "only the root's own file is counted");
        assert_eq!(shallow.bytes, 4);
        assert_eq!(shallow.folders, 2, "the root and `a`");

        let full = scan(&root, &ScanOptions { max_depth: usize::MAX, threads: 2 });
        assert_eq!(full.files, 2);
        assert_eq!(full.bytes, 20);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_root_reports_nothing_but_stays_a_folder() {
        let root = fixture("missing");
        let missing = root.join("gone");
        let scan = super::scan(&missing, &ScanOptions { max_depth: usize::MAX, threads: 4 });
        assert!(!scan.root_readable);
        assert_eq!(scan.errors, 1);
        assert_eq!(scan.files, 0);
        assert_eq!(scan.folders, 1);
        assert!(scan.top.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
