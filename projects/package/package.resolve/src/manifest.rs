use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{PackageError, ProjectLayout, Result};

/// 包清单里的注册表发布配置（`publishConfig`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishConfig {
    /// 注册表 id；空表示继承 CLI / 工作区默认值。
    #[serde(default)]
    pub registry: String,
    /// npm 风格的访问级别（`public` / `restricted`）。
    #[serde(default = "default_access")]
    pub access: String,
    /// 发布 dist-tag（默认 `latest`）。
    #[serde(default = "default_tag")]
    pub tag: String,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self { registry: String::new(), access: default_access(), tag: default_tag() }
    }
}

fn default_access() -> String {
    "public".to_string()
}

fn default_tag() -> String {
    "latest".to_string()
}

/// 包清单 `publish: [ { target, type, package_id, version } ]` 中的一项。
///
/// `type` 是注册表渠道 id；`web-app` / `apk` 一类纯产物格式不进入注册表发布路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PublishTarget {
    /// 该发布关联的构建目标（`wasm`、`clr` 等）。
    #[serde(default)]
    pub target: String,
    /// 注册表适配器名（渠道 id）。
    #[serde(rename = "type", default)]
    pub format_type: String,
    /// 在注册表上的包身份。
    #[serde(default)]
    pub package_id: String,
    /// 发布版本；为空或 `workspace` 时回落到清单 / 工作区版本。
    #[serde(default)]
    pub version: String,
}

impl PublishTarget {
    /// `type` 是否指向本 crate 认识的内置注册表渠道。
    pub fn is_registry_target(&self) -> bool {
        is_registry_publish_type(&self.format_type)
    }

    /// `type` 是否指向注册表渠道，允许宿主追加自有渠道 id。
    pub fn is_registry_target_any(&self, extra: &[&str]) -> bool {
        is_registry_publish_type_any(&self.format_type, extra)
    }
}

/// 内存中的包清单视图。
///
/// 磁盘文件名由 [`ProjectLayout`] 提供；加载后与具体格式无关（当前为 VON）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageManifest {
    /// 包名。
    pub name: String,
    /// 语义版本或工作区继承版本。
    #[serde(default = "default_version")]
    pub version: String,
    /// 简短描述。
    #[serde(default, deserialize_with = "optional_string")]
    pub description: Option<String>,
    /// 项目主页。
    #[serde(default, deserialize_with = "optional_string")]
    pub homepage: Option<String>,
    /// 作者或维护者。
    #[serde(default, deserialize_with = "optional_string")]
    pub author: Option<String>,
    /// SPDX 许可证标识。
    #[serde(default, deserialize_with = "optional_string")]
    pub license: Option<String>,
    /// 运行时依赖。
    #[serde(default)]
    pub dependencies: BTreeMap<String, DependencySpec>,
    /// 开发依赖。
    #[serde(default)]
    pub dev_dependencies: BTreeMap<String, DependencySpec>,
    /// 同级包需满足的 peer 依赖。
    #[serde(rename = "peerDependencies", default)]
    pub peer_dependencies: BTreeMap<String, DependencySpec>,
    /// 生命周期脚本（如 `prePublish`）。
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    /// 钩子命令（与 `scripts` 并列的遗留字段）。
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
    /// 默认发布注册表与 dist-tag 配置。
    #[serde(rename = "publishConfig", default)]
    pub publish_config: PublishConfig,
    /// 注册表 / 渠道发布目标（`publish: [...]`）。
    #[serde(default)]
    pub publish: Vec<PublishTarget>,
    /// 追加进注册表 tarball 的文件 glob（相对工程根）。
    #[serde(default)]
    pub files: Vec<String>,
}

/// `type` 是否指向本 crate 认识的内置注册表渠道。
pub fn is_registry_publish_type(format_type: &str) -> bool {
    is_registry_publish_type_any(format_type, &[])
}

/// `type` 是否指向注册表渠道，`extra` 供宿主登记自有渠道 id。
pub fn is_registry_publish_type_any(format_type: &str, extra: &[&str]) -> bool {
    let value = format_type.trim().to_ascii_lowercase();
    matches!(value.as_str(), "npm" | "jsr" | "nuget" | "maven" | "conda") || extra.iter().any(|item| item.eq_ignore_ascii_case(&value))
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// 接受裸字符串或 `none` 的可选字段。
///
/// VON 文本里省略该键或直接写字符串都是常见写法，读取时不要求 `Some(...)` 包装。
fn optional_string<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Deserialize, Deserializer, Visitor};
    use std::fmt;

    struct OptionalString;

    impl<'de> Visitor<'de> for OptionalString {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or none")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
            Ok(Some(value.to_string()))
        }

        fn visit_string<E: serde::de::Error>(self, value: String) -> std::result::Result<Self::Value, E> {
            Ok(Some(value))
        }

        fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(self, inner: D2) -> std::result::Result<Self::Value, D2::Error> {
            String::deserialize(inner).map(Some)
        }
    }

    deserializer.deserialize_any(OptionalString)
}

/// 依赖声明：workspace 协议、版本约束或带 path/registry 的对象形式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySpec {
    /// 使用 workspace 成员版本（`workspace:*`）。
    Workspace,
    /// 纯版本范围字符串（`^1.0`、`1.2.3` 等）。
    Version(String),
    /// 结构化依赖，可指定 path、registry 与 version。
    Detailed {
        /// 版本范围或精确版本。
        version: Option<String>,
        /// 本地 path 依赖路径。
        path: Option<String>,
        /// 覆盖默认注册表 id。
        registry: Option<String>,
    },
}

impl DependencySpec {
    /// 返回用于解析的版本约束字符串。
    pub fn version_constraint(&self) -> Option<&str> {
        match self {
            Self::Workspace => Some("workspace:*"),
            Self::Version(version) => Some(version.as_str()),
            Self::Detailed { version, .. } => version.as_deref(),
        }
    }

    /// 是否为 workspace 协议依赖。
    pub fn is_workspace(&self) -> bool {
        matches!(self, Self::Workspace) || matches!(self, Self::Version(value) if value.starts_with("workspace:"))
    }
}

impl Serialize for DependencySpec {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Workspace => true.serialize(serializer),
            Self::Version(version) => version.serialize(serializer),
            Self::Detailed { version, path, registry } => {
                DetailedDependency { version: version.clone(), path: path.clone(), registry: registry.clone() }.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for DependencySpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match DependencySpecDef::deserialize(deserializer)? {
            DependencySpecDef::Bool(true) => Ok(Self::Workspace),
            DependencySpecDef::Bool(false) => Ok(Self::Version("false".to_string())),
            DependencySpecDef::String(version) if version == "workspace:*" || version.starts_with("workspace:") => Ok(Self::Workspace),
            DependencySpecDef::String(version) => Ok(Self::Version(version)),
            DependencySpecDef::Detailed(value) => Ok(Self::Detailed { version: value.version, path: value.path, registry: value.registry }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DetailedDependency {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    registry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum DependencySpecDef {
    Bool(bool),
    String(String),
    Detailed(DetailedDependency),
}

/// 依赖所属的清单分组（运行时 / 开发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DependencyBucket {
    /// `dependencies`（运行时）。
    Runtime,
    /// `dev_dependencies`。
    Dev,
}

impl PackageManifest {
    /// `directory` 下的包清单路径。
    pub fn path_in(directory: impl AsRef<Path>, layout: ProjectLayout) -> PathBuf {
        layout.package_manifest_path(directory)
    }

    /// 从磁盘加载并解析包清单。
    pub fn load(directory: impl AsRef<Path>, layout: ProjectLayout) -> Result<Self> {
        let path = Self::path_in(directory, layout);
        let source = std::fs::read_to_string(&path)?;
        Self::parse(&source)
    }

    /// 从 VON 源码解析包清单。
    pub fn parse(source: &str) -> Result<Self> {
        data_text_von::from_str(source).map_err(PackageError::von)
    }

    /// 包清单是否存在于 `directory`。
    pub fn exists(directory: impl AsRef<Path>, layout: ProjectLayout) -> bool {
        Self::path_in(directory, layout).is_file()
    }

    /// 按布局的清单文件名写回缩进 VON。
    ///
    /// 持有原生清单（例如 `package.json`）的产品应让管理器以 `write_manifest=false` 运行，
    /// 再由自己的桥接层持久化。
    pub fn save(&self, directory: impl AsRef<Path>, layout: ProjectLayout) -> Result<()> {
        let path = Self::path_in(directory, layout);
        let content = crate::von::render_manifest(self);
        std::fs::write(path, content)?;
        Ok(())
    }

    /// `name` 当前所在的依赖分组，同时存在时优先运行时。
    pub fn dependency_bucket(&self, name: &str) -> Option<DependencyBucket> {
        if self.dependencies.contains_key(name) {
            Some(DependencyBucket::Runtime)
        }
        else if self.dev_dependencies.contains_key(name) {
            Some(DependencyBucket::Dev)
        }
        else {
            None
        }
    }

    /// 记入 `dependencies`（若在 `dev_dependencies` 中则移除）。
    pub fn add_dependency(&mut self, name: impl Into<String>, version: impl Into<String>) {
        self.set_dependency(name, version, DependencyBucket::Runtime);
    }

    /// 记入 `dev_dependencies`（若在 `dependencies` 中则移除）。
    pub fn add_dev_dependency(&mut self, name: impl Into<String>, version: impl Into<String>) {
        self.set_dependency(name, version, DependencyBucket::Dev);
    }

    /// 把 `name@version` 写入指定分组，并从另一分组移除。
    pub fn set_dependency(&mut self, name: impl Into<String>, version: impl Into<String>, bucket: DependencyBucket) {
        let name = name.into();
        let spec = DependencySpec::Version(version.into());
        match bucket {
            DependencyBucket::Runtime => {
                self.dev_dependencies.remove(&name);
                self.dependencies.insert(name, spec);
            }
            DependencyBucket::Dev => {
                self.dependencies.remove(&name);
                self.dev_dependencies.insert(name, spec);
            }
        }
    }

    /// 在 `name` 已有的分组内更新版本；缺失时按运行时处理。
    pub fn upsert_dependency_preserving_bucket(&mut self, name: impl Into<String>, version: impl Into<String>) {
        let name = name.into();
        let bucket = self.dependency_bucket(&name).unwrap_or(DependencyBucket::Runtime);
        self.set_dependency(name, version, bucket);
    }

    /// 从运行时 / 开发任一依赖表移除 `name`。
    pub fn remove_dependency(&mut self, name: &str) -> bool {
        self.dependencies.remove(name).is_some() || self.dev_dependencies.remove(name).is_some()
    }
}
