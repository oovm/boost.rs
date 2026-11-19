use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{PackageError, PackageManifest, ProjectLayout, Result};

/// 工作区清单（成员列表）；磁盘文件名由 [`ProjectLayout`] 指定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceManifest {
    /// 工作区名称。
    #[serde(default)]
    pub name: String,
    /// 工作区统一版本（成员可继承）。
    #[serde(default)]
    pub version: String,
    /// 成员目录相对路径列表。
    #[serde(default)]
    pub members: Vec<String>,
}

impl WorkspaceManifest {
    /// `directory` 下的工作区清单路径。
    pub fn path_in(directory: impl AsRef<Path>, layout: ProjectLayout) -> PathBuf {
        layout.workspace_manifest_path(directory)
    }

    /// 工作区清单是否存在于 `directory`。
    pub fn exists(directory: impl AsRef<Path>, layout: ProjectLayout) -> bool {
        Self::path_in(directory, layout).is_file()
    }

    /// 从磁盘加载工作区清单。
    pub fn load(directory: impl AsRef<Path>, layout: ProjectLayout) -> Result<Self> {
        let source = std::fs::read_to_string(Self::path_in(directory, layout))?;
        data_text_von::from_str(&source).map_err(PackageError::von)
    }

    /// 解析成员路径（相对工作区根，非递归）。
    pub fn member_paths(&self, root: impl AsRef<Path>) -> Vec<PathBuf> {
        let root = root.as_ref();
        self.members.iter().map(|member| root.join(member)).collect()
    }

    /// 递归枚举含包清单的成员目录。
    pub fn enumerate_member_dirs(&self, root: impl AsRef<Path>, layout: ProjectLayout) -> Result<Vec<PathBuf>> {
        let mut members = Vec::new();
        self.collect_member_dirs(root.as_ref(), layout, &mut members)?;
        Ok(members)
    }

    fn collect_member_dirs(&self, workspace_root: &Path, layout: ProjectLayout, members: &mut Vec<PathBuf>) -> Result<()> {
        for member in &self.members {
            let member_dir = workspace_root.join(member);
            if Self::exists(&member_dir, layout) {
                let nested_workspace_manifest = Self::load(&member_dir, layout)?;
                nested_workspace_manifest.collect_member_dirs(&member_dir, layout, members)?;
            }
            if PackageManifest::exists(&member_dir, layout) {
                members.push(member_dir);
            }
        }
        Ok(())
    }

    /// 包名到成员目录的映射（递归，含目录名别名）。
    pub fn member_packages(&self, root: impl AsRef<Path>, layout: ProjectLayout) -> Result<BTreeMap<String, PathBuf>> {
        let mut map = BTreeMap::new();
        for path in self.enumerate_member_dirs(root, layout)? {
            let manifest = PackageManifest::load(&path, layout)?;
            map.insert(manifest.name.clone(), path.clone());
            if let Some(basename) = path.file_name().and_then(|name| name.to_str()) {
                map.entry(basename.to_string()).or_insert(path);
            }
        }
        Ok(map)
    }
}

/// 由目录结构发现的项目模式。
#[derive(Debug, Clone)]
pub enum ProjectMode {
    /// 根目录含工作区清单；`manifest` 为根包清单（若存在）。
    Workspace {
        /// 工程根目录。
        root: PathBuf,
        /// 已加载的工作区清单。
        workspace: WorkspaceManifest,
        /// 根目录上的可选包清单。
        manifest: Option<PackageManifest>,
    },
    /// 单包工程：根目录仅有包清单。
    Package {
        /// 工程根目录。
        root: PathBuf,
        /// 包清单。
        manifest: PackageManifest,
    },
    /// 既无工作区清单也无包清单的裸目录。
    Standalone {
        /// 目录路径。
        root: PathBuf,
    },
}

impl ProjectMode {
    /// 探测 `directory` 是工作区、单包还是裸目录。
    pub fn discover(directory: impl AsRef<Path>, layout: ProjectLayout) -> Result<Self> {
        let root = directory.as_ref().to_path_buf();
        if WorkspaceManifest::exists(&root, layout) {
            let workspace = WorkspaceManifest::load(&root, layout)?;
            let manifest = if PackageManifest::exists(&root, layout) { Some(PackageManifest::load(&root, layout)?) } else { None };
            return Ok(Self::Workspace { root, workspace, manifest });
        }
        if PackageManifest::exists(&root, layout) {
            return Ok(Self::Package { manifest: PackageManifest::load(&root, layout)?, root });
        }
        Ok(Self::Standalone { root })
    }

    /// 工程根目录。
    pub fn root(&self) -> &Path {
        match self {
            Self::Workspace { root, .. } | Self::Package { root, .. } | Self::Standalone { root } => root,
        }
    }

    /// 当前模式下的包清单；裸目录返回 `None`。
    pub fn package_manifest(&self) -> Option<&PackageManifest> {
        match self {
            Self::Workspace { manifest, .. } => manifest.as_ref(),
            Self::Package { manifest, .. } => Some(manifest),
            Self::Standalone { .. } => None,
        }
    }

    /// 要求存在包清单，否则返回错误。
    pub fn expect_package_manifest(&self) -> Result<&PackageManifest> {
        self.package_manifest().ok_or_else(|| PackageError::message("当前目录不是包（缺少包清单）"))
    }
}
