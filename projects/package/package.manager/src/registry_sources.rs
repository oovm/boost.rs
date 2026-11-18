use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use package_registry::Registry;
use package_resolve::ProjectLayout;
use serde::{Deserialize, Serialize};

use crate::{ManagerError, Result};

/// 注册表适配器工厂：把 endpoint 表解析为可用的渠道集合。
pub type RegistryFactory = Arc<dyn Fn(&BTreeMap<String, String>) -> std::result::Result<HashMap<String, Arc<dyn Registry>>, package_registry::RegistryError> + Send + Sync>;

/// 默认工厂：只构建本 crate 认识的第三方渠道。
pub fn default_registry_factory() -> RegistryFactory {
    Arc::new(|endpoints: &BTreeMap<String, String>| package_registry::registries_with_endpoints(endpoints))
}

/// 持久化的注册表 endpoint 配置。
pub struct RegistrySourceManager {
    sources: BTreeMap<String, String>,
    path: PathBuf,
    factory: RegistryFactory,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SourcesFile {
    #[serde(default)]
    sources: BTreeMap<String, String>,
}

impl RegistrySourceManager {
    /// 使用工程布局打开（默认工厂）。
    pub fn open_with_layout(layout: ProjectLayout) -> Result<Self> {
        Self::open_with_factory(layout.registry_sources_path(), default_registry_factory())
    }

    /// 使用工程布局与显式工厂打开。
    pub fn open_with_factory(path: impl AsRef<Path>, factory: RegistryFactory) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut manager = Self { sources: defaults(&factory)?, path, factory };
        manager.load()?;
        Ok(manager)
    }

    /// 当前打开的 `registry-sources.von` 路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 注册表名到 endpoint 的只读映射。
    pub fn sources(&self) -> &BTreeMap<String, String> {
        &self.sources
    }

    /// 查询单个注册表 endpoint；名称不区分大小写。
    pub fn get_endpoint(&self, registry_name: &str) -> Option<&str> {
        self.sources.get(&registry_name.to_ascii_lowercase()).map(String::as_str)
    }

    /// 列出所有 `(registry, endpoint)` 配置。
    pub fn list(&self) -> Vec<(String, String)> {
        self.sources.iter().map(|(name, endpoint)| (name.clone(), endpoint.clone())).collect()
    }

    /// 新增或覆盖注册表 endpoint 并持久化。
    pub fn add(&mut self, registry_name: &str, endpoint: &str) -> Result<()> {
        let name = registry_name.to_ascii_lowercase();
        if name.trim().is_empty() {
            return Err(ManagerError::message("注册表名称不能为空"));
        }
        let endpoint = endpoint.trim().trim_end_matches('/');
        if endpoint.is_empty() {
            return Err(ManagerError::message("注册表 endpoint 不能为空"));
        }
        self.sources.insert(name, endpoint.to_string());
        self.save()
    }

    /// 删除自定义 endpoint；内置注册表恢复默认值。
    pub fn remove(&mut self, registry_name: &str) -> Result<bool> {
        let name = registry_name.to_ascii_lowercase();
        let defaults = defaults(&self.factory)?;
        if let Some(default) = defaults.get(&name).cloned() {
            self.sources.insert(name, default);
            self.save()?;
            return Ok(true);
        }
        let removed = self.sources.remove(&name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// 返回 `(名称, endpoint, 是否为内置默认值)`。
    pub fn info(&self, registry_name: &str) -> Result<(String, String, bool)> {
        let name = registry_name.to_ascii_lowercase();
        let endpoint = self.get_endpoint(&name).ok_or_else(|| ManagerError::message(format!("注册表 {name} 未配置")))?.to_string();
        let is_default = defaults(&self.factory)?.get(&name).map(String::as_str) == Some(endpoint.as_str());
        Ok((name, endpoint, is_default))
    }

    /// 根据当前 endpoint 表实例化所有注册表适配器。
    pub fn build_registries(&self) -> Result<HashMap<String, Arc<dyn Registry>>> {
        (self.factory)(&self.sources).map_err(ManagerError::from)
    }

    fn load(&mut self) -> Result<()> {
        if !self.path.is_file() {
            self.sources = defaults(&self.factory)?;
            return Ok(());
        }
        let source = std::fs::read_to_string(&self.path)?;
        if source.trim().is_empty() {
            self.sources = defaults(&self.factory)?;
            return Ok(());
        }
        let file: SourcesFile = oak_von::from_str(&source).unwrap_or_else(|_| {
            oak_von::from_str::<BTreeMap<String, String>>(&source).map(|sources| SourcesFile { sources }).unwrap_or_default()
        });
        self.sources = defaults(&self.factory)?;
        for (name, endpoint) in file.sources {
            self.sources.insert(name.to_ascii_lowercase(), endpoint.trim_end_matches('/').to_string());
        }
        Ok(())
    }

    /// 把 endpoint 表写回 `registry-sources.von`。
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, render_sources(&self.sources))?;
        Ok(())
    }
}

fn defaults(factory: &RegistryFactory) -> Result<BTreeMap<String, String>> {
    let registries = factory(&BTreeMap::new()).map_err(ManagerError::from)?;
    Ok(registries.into_iter().map(|(name, registry)| (name, registry.endpoint().to_string())).collect())
}

/// `registry-sources.von` 的写入形状：读取端只接受 `key: "value"`。
fn render_sources(sources: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{\n");
    if !sources.is_empty() {
        out.push_str("    sources: {\n");
        for (name, endpoint) in sources {
            out.push_str(&format!("        \"{}\": \"{}\",\n", escape(name), escape(endpoint)));
        }
        out.push_str("    },\n");
    }
    out.push_str("}\n");
    out
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
