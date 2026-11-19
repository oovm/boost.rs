//! 工作区级内容寻址缓存与摘要工具。
//!
//! 只提供 Legion 需要的机制：稳定摘要、逐文件摘要、按 triple 分桶的 blob 存储。
//! 不拥有源码 staging、词法分析或产物语义。

use std::{
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// 把若干字符串按分隔符摘要成 64 位小写 hex。
pub fn combined_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    to_hex(&hasher.finalize())
}

/// 逐文件摘要：路径（规范化的 `/` 分隔）与内容都进入摘要。
pub fn files_hash(files: &[PathBuf]) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    for path in files {
        let display = path.to_string_lossy().replace('\\', "/");
        hasher.update(display.as_bytes());
        hasher.update([0u8]);
        let bytes = fs::read(path)?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 从 `start` 向上查找包含 `marker` 的目录。
pub fn resolve_marker_root(start: impl AsRef<Path>, marker: &str) -> Option<PathBuf> {
    let mut cursor = Some(start.as_ref());
    while let Some(directory) = cursor {
        if directory.join(marker).is_file() {
            return Some(directory.to_path_buf());
        }
        cursor = directory.parent();
    }
    None
}

/// 按 `{root}/{triple}/{kind}/{key}.bin` 存放的 blob 缓存。
#[derive(Debug, Clone)]
pub struct WorkspaceCache {
    /// 缓存根目录。
    pub root: PathBuf,
}

impl WorkspaceCache {
    /// 以 `root` 为缓存根打开（不创建目录）。
    pub fn open(root: impl AsRef<Path>) -> Self {
        Self { root: root.as_ref().to_path_buf() }
    }

    fn entry_path(&self, triple: &str, kind: &str, key: &str) -> PathBuf {
        self.root.join(sanitize(triple)).join(sanitize(kind)).join(format!("{}.bin", sanitize(key)))
    }

    /// 读取一条缓存；缺失返回 `Ok(None)`。
    pub fn get(&self, triple: &str, key: &str, kind: &str) -> std::io::Result<Option<Vec<u8>>> {
        match fs::read(self.entry_path(triple, kind, key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// 写入一条缓存（父目录按需创建）。
    pub fn put(&self, triple: &str, key: &str, kind: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.entry_path(triple, kind, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)
    }

    /// 删除一个 triple 桶。
    pub fn invalidate_bucket(&self, triple: &str) -> std::io::Result<()> {
        let path = self.root.join(sanitize(triple));
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// 删除整个缓存根。
    pub fn invalidate_all(&self) -> std::io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// 把任意身份串收敛成安全文件/目录名（非字母数字 `.` `-` `_` 一律替换）。
fn sanitize(value: &str) -> String {
    value.chars().map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') { ch } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_hash_is_stable_and_separated() {
        assert_eq!(combined_hash(&["a", "bc"]), combined_hash(&["a", "bc"]));
        assert_ne!(combined_hash(&["a", "bc"]), combined_hash(&["ab", "c"]));
        assert_eq!(combined_hash(&["x"]).len(), 64);
    }

    #[test]
    fn marker_root_walks_upward() {
        let dir = boost_fs::temp::create_folder().expect("temp");
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("nested");
        fs::write(dir.path().join("legions.von"), "").expect("marker");
        assert_eq!(resolve_marker_root(&nested, "legions.von"), Some(dir.path().to_path_buf()));
        assert_eq!(resolve_marker_root(&nested, "missing.von"), None);
    }
}
