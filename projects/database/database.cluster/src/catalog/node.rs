//! 节点这次启动用什么路由：显式单节点，还是调用方显式给出的 catalog 文件。
//!
//! 两个来源必须区分开：`None` 是调用方明确选择的单节点模式，合法地只有本地分片；
//! 给了路径却读不到、解析失败、schema 不合法或没有发布路由，都属于启动失败，
//! 不允许告警后改用本地分片继续运行。

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use database_types::cluster::{Error, ShardEpoch, ShardId};

use crate::{catalog::Catalog, topology::ShardMap};

/// 显式单节点模式使用的本地分片 id。
pub const LOCAL_SHARD_ID: &str = "local-0";

/// 单节点模式发布的唯一路由世代。
const LOCAL_ROUTING_EPOCH: u64 = 1;

/// 节点这次启动的路由来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRouting {
    /// 调用方显式选择单节点模式，路由图只有 `local-0`。
    Local(ShardMap),
    /// catalog 已发布的路由图。
    Catalog(ShardMap),
}

impl NodeRouting {
    /// 路由图；两种来源形状一致，调用方不需要按来源分支。
    pub fn map(&self) -> &ShardMap {
        match self {
            Self::Local(map) | Self::Catalog(map) => map,
        }
    }

    /// 取出路由图本身。
    pub fn into_map(self) -> ShardMap {
        match self {
            Self::Local(map) | Self::Catalog(map) => map,
        }
    }
}

/// 一个节点目录解析出的 catalog 事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCatalogFacts {
    /// catalog 发布的 schema 版本；显式单节点模式为 `None`。
    pub schema_revision: Option<u32>,
    /// 路由来源与路由图。
    pub routing: NodeRouting,
}

/// 读取显式给出的 catalog 失败。
#[derive(Debug)]
pub enum CatalogLoadError {
    /// 调用方给出的 catalog 路径不存在。
    NotFound {
        /// 调用方给出的路径。
        path: PathBuf,
    },
    /// 读取 catalog 时发生的 I/O 失败，保留原始错误。
    Io {
        /// 调用方给出的路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        source: io::Error,
    },
    /// catalog 字节无法解析为已发布的 catalog 布局。
    Parse {
        /// 调用方给出的路径。
        path: PathBuf,
        /// 解析失败细节。
        message: String,
    },
    /// catalog 的 schema 文档未通过校验。
    Schema {
        /// 调用方给出的路径。
        path: PathBuf,
        /// 校验失败细节。
        message: String,
    },
    /// catalog 存在且可解析，但没有发布任何路由世代。
    RoutingMissing {
        /// 调用方给出的路径。
        path: PathBuf,
    },
}

impl CatalogLoadError {
    /// 调用方给出的 catalog 路径。
    pub fn path(&self) -> &Path {
        match self {
            Self::NotFound { path } | Self::Io { path, .. } | Self::Parse { path, .. } | Self::Schema { path, .. } | Self::RoutingMissing { path } => path,
        }
    }

    /// 机器可读的失败类别：`not_found` / `io` / `parse` / `schema` / `routing_missing`。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Io { .. } => "io",
            Self::Parse { .. } => "parse",
            Self::Schema { .. } => "schema",
            Self::RoutingMissing { .. } => "routing_missing",
        }
    }
}

impl fmt::Display for CatalogLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "catalog file {} not found", path.display()),
            Self::Io { path, source } => write!(formatter, "catalog I/O error on {}: {source}", path.display()),
            Self::Parse { path, message } => write!(formatter, "catalog {} could not be parsed: {message}", path.display()),
            Self::Schema { path, message } => write!(formatter, "catalog {} schema rejected: {message}", path.display()),
            Self::RoutingMissing { path } => write!(formatter, "catalog {} has no published routing epoch", path.display()),
        }
    }
}

impl std::error::Error for CatalogLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 读取并校验节点这次启动使用的路由事实。
///
/// `catalog_path` 为 `None` 表示调用方显式选择单节点模式；为 `Some` 时必须读到
/// 一个带已发布路由的 catalog，任何失败都原样返回，不回退成本地分片。
pub fn load_node_catalog(catalog_path: Option<&Path>) -> Result<NodeCatalogFacts, CatalogLoadError> {
    let Some(path) = catalog_path else {
        let map = local_shard_map();
        return Ok(NodeCatalogFacts { schema_revision: None, routing: NodeRouting::Local(map) });
    };
    if !path.exists() {
        return Err(CatalogLoadError::NotFound { path: path.to_path_buf() });
    }
    let catalog = Catalog::open(path).map_err(|error| classify(path, error))?;
    let map = catalog.routing().map_err(|error| classify(path, error))?.ok_or_else(|| CatalogLoadError::RoutingMissing { path: path.to_path_buf() })?;
    Ok(NodeCatalogFacts { schema_revision: catalog.schema().map(|schema| schema.version), routing: NodeRouting::Catalog(map) })
}

/// 单节点模式的路由图。
fn local_shard_map() -> ShardMap {
    ShardMap::new(ShardEpoch(LOCAL_ROUTING_EPOCH), vec![ShardId(LOCAL_SHARD_ID.to_owned())]).expect("the built-in single-node routing map is valid")
}

fn classify(path: &Path, error: Error) -> CatalogLoadError {
    let path = path.to_path_buf();
    match error {
        Error::Io(source) => CatalogLoadError::Io { path, source },
        Error::Corrupt(message) => CatalogLoadError::Parse { path, message: message.to_owned() },
        Error::CorruptReason { reason, message } => CatalogLoadError::Parse { path, message: format!("{reason}: {message}") },
        Error::Schema { message } => CatalogLoadError::Schema { path, message },
        Error::SchemaConflict { expected, found } => {
            CatalogLoadError::Schema { path, message: format!("catalog schema conflict: expected {expected}, found {found}") }
        }
        other => CatalogLoadError::Parse { path, message: other.to_string() },
    }
}
