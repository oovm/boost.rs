use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{CacheError, Result, von};

/// 已安装包的本地索引缓存，根目录由调用方决定。
#[derive(Debug, Clone)]
pub struct PackageCache {
    root: PathBuf,
    index: CacheIndex,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheIndex {
    #[serde(default)]
    packages: BTreeMap<String, String>,
}

impl PackageCache {
    /// 打开或初始化 `root` 下的缓存目录，并加载 `cache-index.von` 索引。
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let index_path = root.join("cache-index.von");
        let index = if index_path.is_file() {
            let source = std::fs::read_to_string(&index_path)?;
            oak_von::from_str(&source).unwrap_or_default()
        }
        else {
            CacheIndex::default()
        };
        Ok(Self { root, index })
    }

    /// 缓存根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `name@version` 是否已写入缓存索引。
    pub fn has_package(&self, name: &str, version: &str) -> bool {
        self.index.packages.contains_key(&format!("{name}@{version}"))
    }

    /// 把本地包目录复制进缓存并更新索引。
    pub fn add_package(&mut self, name: &str, version: &str, package_path: impl AsRef<Path>) -> Result<()> {
        let key = format!("{name}@{version}");
        let target = self.root.join(&key);
        if target.exists() {
            std::fs::remove_dir_all(&target).map_err(|source| CacheError::io(&target, source))?;
        }
        copy_dir(package_path.as_ref(), &target)?;
        self.index.packages.insert(key, target.display().to_string());
        self.save_index()
    }

    /// 已缓存包的解压目录路径。
    pub fn package_path(&self, name: &str, version: &str) -> Option<PathBuf> {
        self.index.packages.get(&format!("{name}@{version}")).map(PathBuf::from)
    }

    /// 清空缓存目录并重置索引文件。
    pub fn clear(&mut self) -> Result<()> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        std::fs::create_dir_all(&self.root)?;
        self.index = CacheIndex::default();
        self.save_index()
    }

    fn save_index(&self) -> Result<()> {
        let path = self.root.join("cache-index.von");
        let content = von::render_package_index(&self.index.packages);
        std::fs::write(&path, content).map_err(|source| CacheError::io(path, source))?;
        Ok(())
    }
}

fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &dest_path)?;
        }
        else {
            std::fs::copy(&source_path, &dest_path)?;
        }
    }
    Ok(())
}
