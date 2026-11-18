//! 工程磁盘布局：包清单、工作区清单、lockfile 与用户数据目录的文件名。
//!
//! 具体产品通过 [`ProjectLayout`] 注入自己的文件名和目录；本 crate 只提供中立的默认值，
//! 不假定任何产品身份。

use std::path::{Path, PathBuf};

/// 定位运行契约产物时尝试的替代物理入口名。
///
/// 产品可以注入历史别名（例如旧命名 → 规范名）；中立布局不提供别名。
pub type EntryAliasFn = fn(&str) -> &'static [&'static str];

/// 发现与持久化包工程时使用的文件名和用户数据位置。
#[derive(Debug, Clone, Copy)]
pub struct ProjectLayout {
    /// 单包清单文件名（例如 `package.von`）。
    pub package_manifest: &'static str,
    /// 工作区清单文件名（例如 `workspace.von`）。
    pub workspace_manifest: &'static str,
    /// 打包忽略文件名（例如 `.packageignore`）。
    pub ignore_file: &'static str,
    /// 工程根下的 lockfile 名（例如 `package-lock.von`）。
    pub lockfile: &'static str,
    /// 用户主目录下的产品数据目录名（例如 `.packages`）。
    pub home_dirname: &'static str,
    /// 覆盖用户主目录的环境变量名。
    pub home_env: &'static str,
    /// 注册表 token 环境变量（在 `{REGISTRY}_TOKEN` 之后查询）。
    pub token_env_vars: &'static [&'static str],
    /// 产品注入的物理入口别名，先于契约名本身被尝试。
    pub entry_aliases: EntryAliasFn,
}

fn no_entry_aliases(_: &str) -> &'static [&'static str] {
    &[]
}

impl ProjectLayout {
    /// 不带任何产品身份的中立默认布局。
    pub const fn neutral() -> Self {
        Self {
            package_manifest: "package.von",
            workspace_manifest: "workspace.von",
            ignore_file: ".packageignore",
            lockfile: "package-lock.von",
            home_dirname: ".package-home",
            home_env: "PACKAGE_HOME",
            token_env_vars: &[],
            entry_aliases: no_entry_aliases,
        }
    }

    /// `directory` 下的包清单路径。
    pub fn package_manifest_path(self, directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(self.package_manifest)
    }

    /// `directory` 下的工作区清单路径。
    pub fn workspace_manifest_path(self, directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(self.workspace_manifest)
    }

    /// `directory` 下的打包忽略文件路径。
    pub fn ignore_file_path(self, directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(self.ignore_file)
    }

    /// `directory` 下的 lockfile 路径。
    pub fn lockfile_path(self, directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(self.lockfile)
    }

    /// 解析用户主目录（`home_env`，其次 `HOME` / `USERPROFILE`）。
    pub fn resolve_user_home(self) -> PathBuf {
        std::env::var_os(self.home_env)
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 产品数据目录（`$home/{home_dirname}`）。
    pub fn product_data_dir(self) -> PathBuf {
        self.resolve_user_home().join(self.home_dirname)
    }

    /// 包与安全审计的内容寻址缓存根。
    pub fn cache_dir(self) -> PathBuf {
        self.product_data_dir().join("cache")
    }

    /// 凭据存储路径（产品数据目录下的 `auth.von`）。
    pub fn auth_store_path(self) -> PathBuf {
        self.product_data_dir().join("auth.von")
    }

    /// 注册表 endpoint 配置路径。
    pub fn registry_sources_path(self) -> PathBuf {
        self.product_data_dir().join("registry-sources.von")
    }
}

impl Default for ProjectLayout {
    fn default() -> Self {
        Self::neutral()
    }
}
