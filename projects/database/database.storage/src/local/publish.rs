//! 文件原子发布：完整写入 temp 文件，再经一次目录项替换提交，失败可恢复。
//!
//! 这是存储层唯一的通用原子发布路径（temp 写 → replace → 失败时把旧文件从
//! 备份位放回）。目录项替换与既有 `.yykv` 快照发布同源：unix 用 `rename`，
//! Windows 用 `MoveFileExW` 带 `REPLACE_EXISTING | WRITE_THROUGH`，因此不存在
//! 「先删除旧文件再改名」的裸窗口；旧文件只会先被移到备份名，替换失败即放回。
//! 上层（如 SQL schema 目录）不再自带第二套 temp/rename/remove 序列。
//!
//! 失败结果必须能回答三个问题，而不是只给一个 `io::Error`：
//! 提交点是否已经越过、旧内容当前是否仍在、此刻能否安全重试。`PublishError`
//! 因此携带阶段 [`PublishStage`]、结果 [`PublishOutcome`] 与可重试事实；恢复
//! 再次失败时备份路径和第二次错误都不会被丢弃。
//!
//! 发布链同时是 yydb 唯一的提交点，因此也是运行期记录的自然汇聚处：
//! [`PUBLISH_SPOTS`] 常开聚合 temp 写落盘、目录项替换与整体发布的耗时
//! （`yydb.publish.temp`、`yydb.publish.replace`、`yydb.publish`），并登记
//! 明确卡点（`replace-blocked`、`restore-unconfirmed`）。记录关闭时单次埋点
//! 只付一次原子读，开启时不改变任何控制流，产品运行不受影响；宿主经
//! [`publish_runtime_snapshot`] 取快照做持续调优。

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use boost_harness::hotspot::{Hotspot, HotspotSnapshot};

/// 发布链的运行期记录器：常开、有界、近零开销。
pub static PUBLISH_SPOTS: LazyLock<Hotspot> = LazyLock::new(Hotspot::new);

/// 当前发布链快照，供宿主诊断接口合并展示。
pub fn publish_runtime_snapshot() -> HotspotSnapshot {
    PUBLISH_SPOTS.snapshot()
}

/// 总开关：测量窗口需要排除记录本身时关闭。
pub fn set_publish_runtime_enabled(enabled: bool) {
    PUBLISH_SPOTS.set_enabled(enabled);
}

/// 失败发生在发布的哪个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStage {
    /// 创建目标目录。
    Prepare,
    /// 写 temp 文件并让它落盘。
    WriteTemp,
    /// 目录项替换，提交点。
    Replace,
    /// 提交后同步父目录，用来确认替换本身耐久。
    SyncParent,
    /// 替换失败后把旧文件从备份位放回。
    Restore,
}

/// 一次失败时目标文件的可观察状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// 提交点未越过，旧内容（或「不存在」）保持不变。
    Unchanged,
    /// 提交点已越过，新内容已可见，但父目录同步失败，耐久性未证实。
    VisibleNotDurable,
    /// 替换失败且旧内容未能放回，目标可能缺席，备份仍在磁盘上。
    RestoreUnconfirmed,
}

/// 发布成功后的回执，成功即表示新内容已可见且父目录已同步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishReceipt {
    /// 已发布的目标路径。
    pub path: PathBuf,
    /// 固定为 [`PublishOutcome::Unchanged`] 之外的成功结果，这里显式记录。
    pub durable: bool,
}

/// 带阶段、结果和可重试事实的发布错误。
#[derive(Debug)]
pub struct PublishError {
    stage: PublishStage,
    outcome: PublishOutcome,
    retryable: bool,
    source: io::Error,
    restore_error: Option<io::Error>,
    backup: Option<PathBuf>,
}

impl PublishError {
    fn new(stage: PublishStage, outcome: PublishOutcome, retryable: bool, source: io::Error) -> Self {
        Self { stage, outcome, retryable, source, restore_error: None, backup: None }
    }

    fn with_backup(mut self, backup: PathBuf) -> Self {
        self.backup = Some(backup);
        self
    }

    fn with_restore(mut self, error: io::Error) -> Self {
        self.restore_error = Some(error);
        self
    }

    /// 失败阶段。
    pub fn stage(&self) -> PublishStage {
        self.stage
    }

    /// 此时目标文件的可观察状态。
    pub fn outcome(&self) -> PublishOutcome {
        self.outcome
    }

    /// 此刻能否安全地原样重试；`VisibleNotDurable` 与 `RestoreUnconfirmed`
    /// 都要求先检查实际文件，不允许盲重试。
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    /// 原始失败原因。
    pub fn source(&self) -> &io::Error {
        &self.source
    }

    /// 把旧内容放回时的第二次失败。
    pub fn restore_error(&self) -> Option<&io::Error> {
        self.restore_error.as_ref()
    }

    /// 旧文件备份仍在磁盘上时的路径。
    pub fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "publish failed at {:?}: {:?} (retryable={}): {}", self.stage, self.outcome, self.retryable, self.source)?;
        if let Some(error) = &self.restore_error {
            write!(formatter, "; restore failed: {error}")?;
        }
        if let Some(path) = &self.backup {
            write!(formatter, "; backup at {}", path.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<PublishError> for io::Error {
    fn from(error: PublishError) -> Self {
        io::Error::new(error.source.kind(), error)
    }
}

impl From<PublishError> for database_types::cluster::Error {
    fn from(error: PublishError) -> Self {
        Self::Io(error.into())
    }
}

/// 发布过程的替换步骤，测试可注入故障。
pub trait PublishSteps {
    /// 把 `temp` 替换为 `destination` 的当前内容。
    ///
    /// 返回 `Err` 当且仅当目录项未被改动且 `temp` 仍然存在，发布层据此决定
    /// 回退动作。
    fn replace(&self, temp: &Path, destination: &Path) -> io::Result<()>;

    /// 让已发生的替换落盘。
    ///
    /// 返回 `Err` 只表示「已提交但耐久性未证实」，发布层不会因此回滚。
    fn sync_parent(&self, destination: &Path) -> io::Result<()>;

    /// 替换失败后先把旧目标挪到备份位，避免恢复时覆盖别的写入。
    fn stash(&self, destination: &Path, backup: &Path) -> io::Result<()> {
        fs::rename(destination, backup)
    }

    /// 第二次替换仍失败时，把备份位放回目标名。
    fn restore(&self, backup: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(backup, destination)
    }
}

/// 生产替换实现，与 `.yykv` 快照发布同一提交语义。
pub struct AtomicRename;

#[cfg(not(windows))]
impl PublishSteps for AtomicRename {
    fn replace(&self, temp: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(temp, destination)
    }

    fn sync_parent(&self, destination: &Path) -> io::Result<()> {
        // `rename` 只让新的目录项在内存里可见，掉电后替换本身仍可能丢失，
        // 提交点必须再同步父目录。
        let parent = destination.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::File::open(parent)?.sync_all()
    }
}

#[cfg(windows)]
impl PublishSteps for AtomicRename {
    fn replace(&self, temp: &Path, destination: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

        fn wide(path: &Path) -> Vec<u16> {
            path.as_os_str().encode_wide().chain(Some(0)).collect()
        }

        // `MoveFileExW` 与 unix 的 `rename` 是同一个提交点：替换目录项，并且
        // `MOVEFILE_WRITE_THROUGH` 让替换本身落盘。
        let result =
            unsafe { MoveFileExW(wide(temp).as_ptr(), wide(destination).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn sync_parent(&self, _destination: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// 以 temp → replace 的序列把 `bytes` 原子发布到 `path`。
///
/// 成功时 `path` 完整携带 `bytes` 且已耐久；任何在提交点之前的失败都让 `path`
/// 保持发布前的旧内容（不存在则保持不存在），temp 与备份文件被清理。提交点之后
/// 仅耐久性未证实时返回 [`PublishOutcome::VisibleNotDurable`]，新内容不回滚。
pub fn publish_atomic(path: &Path, bytes: &[u8]) -> Result<PublishReceipt, PublishError> {
    publish_with(path, bytes, &AtomicRename)
}

/// 用调用方拥有的 temp 路径发布，供 `.yykv` 等已有私有临时名的一方复用同一提交链。
pub fn publish_with_temp(path: &Path, bytes: &[u8], temp: PathBuf) -> Result<PublishReceipt, PublishError> {
    publish_with_steps(path, bytes, temp, &AtomicRename)
}

fn publish_with(path: &Path, bytes: &[u8], steps: &dyn PublishSteps) -> Result<PublishReceipt, PublishError> {
    let temp = sibling(path, &format!(".tmp-{}", unique_suffix()));
    publish_with_steps(path, bytes, temp, steps)
}

fn publish_with_steps(path: &Path, bytes: &[u8], temp: PathBuf, steps: &dyn PublishSteps) -> Result<PublishReceipt, PublishError> {
    let started = Instant::now();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| PublishError::new(PublishStage::Prepare, PublishOutcome::Unchanged, true, error))?;
    }
    let result = write_and_replace(path, bytes, &temp, steps);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    PUBLISH_SPOTS.record("yydb.publish", started.elapsed());
    result
}

fn write_and_replace(path: &Path, bytes: &[u8], temp: &Path, steps: &dyn PublishSteps) -> Result<PublishReceipt, PublishError> {
    let temp_started = Instant::now();
    let mut file =
        fs::File::create(temp).map_err(|error| PublishError::new(PublishStage::WriteTemp, PublishOutcome::Unchanged, true, error))?;
    file.write_all(bytes).map_err(|error| PublishError::new(PublishStage::WriteTemp, PublishOutcome::Unchanged, true, error))?;
    file.sync_all().map_err(|error| PublishError::new(PublishStage::WriteTemp, PublishOutcome::Unchanged, true, error))?;
    drop(file);
    PUBLISH_SPOTS.record("yydb.publish.temp", temp_started.elapsed());
    let replace_started = Instant::now();
    let replaced = steps.replace(temp, path);
    PUBLISH_SPOTS.record("yydb.publish.replace", replace_started.elapsed());
    match replaced {
        Ok(()) => match steps.sync_parent(path) {
            Ok(()) => Ok(PublishReceipt { path: path.to_path_buf(), durable: true }),
            Err(error) => Err(PublishError::new(PublishStage::SyncParent, PublishOutcome::VisibleNotDurable, false, error)),
        },
        Err(first) => {
            PUBLISH_SPOTS.stall("yydb.publish", "replace-blocked", format!("目录项替换失败，进入备份路径：{first}"));
            recover_with_backup(path, temp, steps, first)
        }
    }
}

fn recover_with_backup(path: &Path, temp: &Path, steps: &dyn PublishSteps, first: io::Error) -> Result<PublishReceipt, PublishError> {
    // 目标可能被另一个打开的句柄占住（Windows）。把旧文件移到备份名
    // 再重试替换，而不是删除它：两次替换都失败时旧内容原样放回。
    if !path.exists() {
        return Err(PublishError::new(PublishStage::Replace, PublishOutcome::Unchanged, true, first));
    }
    let backup = sibling(path, &format!(".bak-{}", unique_suffix()));
    if let Err(error) = steps.stash(path, &backup) {
        // 目标没有被挪走，旧内容仍可读；这次失败可以安全重试。
        return Err(PublishError::new(PublishStage::Replace, PublishOutcome::Unchanged, true, first).with_backup(backup).with_restore(error));
    }
    match steps.replace(temp, path) {
        Ok(()) => match steps.sync_parent(path) {
            Ok(()) => {
                let _ = fs::remove_file(&backup);
                Ok(PublishReceipt { path: path.to_path_buf(), durable: true })
            }
            Err(error) => {
                let _ = fs::remove_file(&backup);
                Err(PublishError::new(PublishStage::SyncParent, PublishOutcome::VisibleNotDurable, false, error))
            }
        },
        Err(second) => {
            let _ = fs::remove_file(temp);
            match steps.restore(&backup, path) {
                Ok(()) => Err(PublishError::new(PublishStage::Replace, PublishOutcome::Unchanged, true, second)),
                Err(error) => {
                    PUBLISH_SPOTS.stall("yydb.publish", "restore-unconfirmed", format!("两次替换失败且放回失败：{second} / {error}"));
                    Err(PublishError::new(PublishStage::Restore, PublishOutcome::RestoreUnconfirmed, false, second)
                        .with_backup(backup)
                        .with_restore(error))
                }
            }
        }
    }
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("storage-publish-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("tempdir");
            Self(dir.join("doc.json"))
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// 目录里除目标文件外的残留，用来证明 temp / 备份已被清理。
        fn leftovers(&self) -> Vec<String> {
            let dir = self.0.parent().expect("parent");
            let mut names = fs::read_dir(dir)
                .expect("read dir")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name != "doc.json")
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }

    /// 替换步骤包装：按次数注入替换、同步、挪备份和放回故障。
    struct Faults {
        replace: usize,
        sync: usize,
        stash: usize,
        restore: usize,
        replace_seen: Cell<usize>,
        sync_seen: Cell<usize>,
        stash_seen: Cell<usize>,
        restore_seen: Cell<usize>,
    }

    impl Faults {
        fn none() -> Self {
            Self {
                replace: 0,
                sync: 0,
                stash: 0,
                restore: 0,
                replace_seen: Cell::new(0),
                sync_seen: Cell::new(0),
                stash_seen: Cell::new(0),
                restore_seen: Cell::new(0),
            }
        }

        fn replace(mut self, failures: usize) -> Self {
            self.replace = failures;
            self
        }

        fn sync(mut self, failures: usize) -> Self {
            self.sync = failures;
            self
        }

        fn stash(mut self, failures: usize) -> Self {
            self.stash = failures;
            self
        }

        fn restore(mut self, failures: usize) -> Self {
            self.restore = failures;
            self
        }
    }

    impl PublishSteps for Faults {
        fn replace(&self, temp: &Path, destination: &Path) -> io::Result<()> {
            self.replace_seen.set(self.replace_seen.get() + 1);
            if self.replace_seen.get() <= self.replace {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected replace failure"));
            }
            AtomicRename.replace(temp, destination)
        }

        fn sync_parent(&self, destination: &Path) -> io::Result<()> {
            self.sync_seen.set(self.sync_seen.get() + 1);
            if self.sync_seen.get() <= self.sync {
                return Err(io::Error::new(io::ErrorKind::Other, "injected sync failure"));
            }
            AtomicRename.sync_parent(destination)
        }

        fn stash(&self, destination: &Path, backup: &Path) -> io::Result<()> {
            self.stash_seen.set(self.stash_seen.get() + 1);
            if self.stash_seen.get() <= self.stash {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected stash failure"));
            }
            AtomicRename.stash(destination, backup)
        }

        fn restore(&self, backup: &Path, destination: &Path) -> io::Result<()> {
            self.restore_seen.set(self.restore_seen.get() + 1);
            if self.restore_seen.get() <= self.restore {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected restore failure"));
            }
            AtomicRename.restore(backup, destination)
        }
    }

    #[test]
    fn publishes_new_and_replaced_content_without_leftovers() {
        let fixture = Fixture::new("write");
        let first = publish_with(fixture.path(), b"first", &AtomicRename).expect("publish");
        assert!(first.durable);
        assert_eq!(fs::read(fixture.path()).expect("read"), b"first");
        publish_with(fixture.path(), b"second", &AtomicRename).expect("republish");
        assert_eq!(fs::read(fixture.path()).expect("reread"), b"second");
        assert!(fixture.leftovers().is_empty(), "leftovers: {:?}", fixture.leftovers());
    }

    #[test]
    fn failure_before_the_replacement_keeps_the_old_file_intact() {
        let fixture = Fixture::new("before-replace");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        // 替换永远失败：temp 写入成功但目录项从未动过，旧内容必须原样保留。
        let steps = Faults::none().replace(usize::MAX);
        let error = publish_with(fixture.path(), b"new", &steps).expect_err("injected failure");
        assert_eq!(error.stage(), PublishStage::Replace);
        assert_eq!(error.outcome(), PublishOutcome::Unchanged);
        assert!(error.retryable());
        assert_eq!(fs::read(fixture.path()).expect("read"), b"old", "old publication survives");
        assert!(fixture.leftovers().is_empty(), "leftovers: {:?}", fixture.leftovers());
    }

    #[test]
    fn a_busy_destination_recovers_through_the_backup_step() {
        let fixture = Fixture::new("busy");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        // 第一次替换失败（目标被句柄占住），移到备份名后的重试成功。
        let steps = Faults::none().replace(1);
        publish_with(fixture.path(), b"new", &steps).expect("publish after backup");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"new");
        assert!(fixture.leftovers().is_empty(), "leftovers: {:?}", fixture.leftovers());
    }

    #[test]
    fn missing_destination_is_created_even_when_the_parent_dir_is_absent() {
        let dir = std::env::temp_dir().join(format!("storage-publish-{}-nested", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("deeper").join("doc.json");
        publish_with(&path, b"nested", &AtomicRename).expect("publish into fresh directory");
        assert_eq!(fs::read(&path).expect("read"), b"nested");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_failure_after_the_replacement_leaves_the_new_content_visible() {
        let fixture = Fixture::new("visible-not-durable");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        let steps = Faults::none().sync(usize::MAX);
        let error = publish_with(fixture.path(), b"new", &steps).expect_err("sync failure");
        assert_eq!(error.stage(), PublishStage::SyncParent);
        assert_eq!(error.outcome(), PublishOutcome::VisibleNotDurable);
        assert!(!error.retryable(), "a visible but unsynced publication must be inspected first");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"new", "the replacement is not rolled back");
        assert!(fixture.leftovers().is_empty(), "leftovers: {:?}", fixture.leftovers());
    }

    #[test]
    fn restore_failure_keeps_the_backup_and_reports_both_causes() {
        let fixture = Fixture::new("restore-unconfirmed");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        // 两次替换都失败，放回也失败：目标缺席，旧内容只能留在备份位。
        let steps = Faults::none().replace(usize::MAX).restore(usize::MAX);
        let error = publish_with(fixture.path(), b"new", &steps).expect_err("restore failure");
        assert_eq!(error.stage(), PublishStage::Restore);
        assert_eq!(error.outcome(), PublishOutcome::RestoreUnconfirmed);
        assert!(!error.retryable());
        assert_eq!(error.source().to_string(), "injected replace failure");
        assert!(error.restore_error().expect("restore cause").to_string().contains("injected restore failure"));
        assert!(!fixture.path().exists(), "the destination is missing until the backup is put back");
        let backup = error.backup().expect("backup path is reported");
        assert_eq!(fs::read(backup).expect("backup content"), b"old");
    }

    #[test]
    fn stash_failure_leaves_the_old_file_at_the_destination() {
        let fixture = Fixture::new("stash");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        let steps = Faults::none().replace(usize::MAX).stash(usize::MAX);
        let error = publish_with(fixture.path(), b"new", &steps).expect_err("stash failure");
        assert_eq!(error.outcome(), PublishOutcome::Unchanged);
        assert!(error.retryable());
        assert_eq!(fs::read(fixture.path()).expect("read"), b"old");
        assert!(error.restore_error().is_some(), "the stash failure is not discarded");
    }

    #[test]
    fn an_unchanged_failure_can_be_retried_after_the_blocker_clears() {
        let fixture = Fixture::new("retry");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        let blocked = Faults::none().replace(usize::MAX);
        let error = publish_with(fixture.path(), b"new", &blocked).expect_err("blocked");
        assert!(error.retryable());
        // 障碍解除后重试：同一目标必须完整换成新内容。
        publish_with(fixture.path(), b"new", &AtomicRename).expect("retry");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"new");
        assert!(fixture.leftovers().is_empty(), "leftovers: {:?}", fixture.leftovers());
    }

    #[test]
    fn publish_chain_records_runtime_hotspots_and_stalls() {
        let fixture = Fixture::new("runtime");
        publish_with(fixture.path(), b"first", &AtomicRename).expect("seed");
        // 第一次替换失败、备份路径成功：既产生耗时行，也产生卡点事件。
        let steps = Faults::none().replace(1);
        publish_with(fixture.path(), b"second", &steps).expect("publish after backup");
        let snapshot = publish_runtime_snapshot();
        let row = snapshot.rows.iter().find(|row| row.key == "yydb.publish").expect("yydb.publish 行");
        assert!(row.hits >= 2, "{row:?}");
        assert!(snapshot.rows.iter().any(|row| row.key == "yydb.publish.temp"), "temp 阶段必须单独成行");
        assert!(snapshot.rows.iter().any(|row| row.key == "yydb.publish.replace"), "替换阶段必须单独成行");
        assert!(snapshot.stalls.iter().any(|event| event.cause == "replace-blocked"), "卡点事件: {:?}", snapshot.stalls);
    }
}
