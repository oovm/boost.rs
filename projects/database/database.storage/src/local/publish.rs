//! 文件原子发布：完整写入 temp 文件，再经一次目录项替换提交，失败可恢复。
//!
//! 这是存储层唯一的通用原子发布路径（temp 写 → replace → 失败时把旧文件从
//! 备份位放回）。目录项替换与既有 `.yykv` 快照发布同源：unix 用 `rename`，
//! Windows 用 `MoveFileExW` 带 `REPLACE_EXISTING | WRITE_THROUGH`，因此不存在
//! 「先删除旧文件再改名」的裸窗口；旧文件只会先被移到备份名，替换失败即放回。
//! 上层（如 SQL schema 目录）不再自带第二套 temp/rename/remove 序列。

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

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
        let result = unsafe {
            MoveFileExW(wide(temp).as_ptr(), wide(destination).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
        };
        if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn sync_parent(&self, _destination: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// 以 temp → replace 的序列把 `bytes` 原子发布到 `path`。
///
/// 成功时 `path` 完整携带 `bytes`；任何在提交点之前的失败都让 `path` 保持
/// 发布前的旧内容（不存在则保持不存在），temp 与备份文件被清理。提交点之后
/// 仅耐久性未证实时返回错误，新内容不回滚。
pub fn publish_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    publish_with(path, bytes, &AtomicRename)
}

fn publish_with(path: &Path, bytes: &[u8], steps: &dyn PublishSteps) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let temp = sibling(path, ".tmp");
    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match steps.replace(&temp, path) {
            Ok(()) => steps.sync_parent(path),
            Err(first) => {
                // 目标可能被另一个打开的句柄占住（Windows）。把旧文件移到备份名
                // 再重试替换，而不是删除它：两次替换都失败时旧内容原样放回。
                if !path.exists() {
                    return Err(first);
                }
                let backup = sibling(path, ".bak");
                fs::rename(path, &backup).map_err(|_| first)?;
                match steps.replace(&temp, path) {
                    Ok(()) => match steps.sync_parent(path) {
                        Ok(()) => {
                            let _ = fs::remove_file(&backup);
                            Ok(())
                        }
                        Err(error) => {
                            let _ = fs::remove_file(&backup);
                            Err(error)
                        }
                    },
                    Err(second) => {
                        let _ = fs::remove_file(&temp);
                        let _ = fs::rename(&backup, path);
                        Err(second)
                    }
                }
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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
            let _ = fs::create_dir_all(&dir);
            Self(dir.join("doc.json"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(sibling(&self.0, ".tmp"));
            let _ = fs::remove_file(sibling(&self.0, ".bak"));
        }
    }

    /// 替换步骤包装：可让前 N 次（或全部）替换失败，模拟目标被占用的窗口。
    struct FlakyReplace {
        failures: usize,
        seen: Cell<usize>,
    }

    impl PublishSteps for FlakyReplace {
        fn replace(&self, temp: &Path, destination: &Path) -> io::Result<()> {
            self.seen.set(self.seen.get() + 1);
            if self.seen.get() <= self.failures {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected replace failure"));
            }
            AtomicRename.replace(temp, destination)
        }

        fn sync_parent(&self, destination: &Path) -> io::Result<()> {
            AtomicRename.sync_parent(destination)
        }
    }

    #[test]
    fn publishes_new_and_replaced_content_without_leftovers() {
        let fixture = Fixture::new("write");
        publish_with(fixture.path(), b"first", &AtomicRename).expect("publish");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"first");
        publish_with(fixture.path(), b"second", &AtomicRename).expect("republish");
        assert_eq!(fs::read(fixture.path()).expect("reread"), b"second");
        assert!(!sibling(fixture.path(), ".tmp").exists());
        assert!(!sibling(fixture.path(), ".bak").exists());
    }

    #[test]
    fn failure_before_the_replacement_keeps_the_old_file_intact() {
        let fixture = Fixture::new("before-replace");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        // 替换永远失败：temp 写入成功但目录项从未动过，旧内容必须原样保留。
        let steps = FlakyReplace { failures: usize::MAX, seen: Cell::new(0) };
        let error = publish_with(fixture.path(), b"new", &steps).expect_err("injected failure");
        assert!(error.to_string().contains("injected"), "unexpected error: {error}");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"old", "old publication survives");
        assert!(!sibling(fixture.path(), ".tmp").exists(), "temp cleaned up");
        assert!(!sibling(fixture.path(), ".bak").exists(), "backup restored and cleaned up");
    }

    #[test]
    fn a_busy_destination_recovers_through_the_backup_step() {
        let fixture = Fixture::new("busy");
        publish_with(fixture.path(), b"old", &AtomicRename).expect("seed");
        // 第一次替换失败（目标被句柄占住），移到备份名后的重试成功。
        let steps = FlakyReplace { failures: 1, seen: Cell::new(0) };
        publish_with(fixture.path(), b"new", &steps).expect("publish after backup");
        assert_eq!(fs::read(fixture.path()).expect("read"), b"new");
        assert!(!sibling(fixture.path(), ".bak").exists(), "backup consumed after success");
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
}
