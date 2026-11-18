//! 节点目录的公开装配与健康快照。
//!
//! catalog 选择与路由校验归 `database.cluster`，分片文件归 `database.storage`，
//! 运行状态文件与停止协调归 `database.server`。本模块只做公开选项映射、资源
//! 组装与字段投影。

use std::{
    fmt,
    path::Path,
    sync::Arc,
};

use database::{
    cluster::{
        catalog::node::{CatalogLoadError, NodeCatalogFacts, load_node_catalog},
        control::{LeaseError, LocalOwnership, NodeIdentity, NodeLease},
    },
    execution::cluster::{KeyValueExecutor, LocalExecutor},
    server::node::{NodeControlError, RuntimeStatusError},
    storage::cluster_kv::open_shards,
    types::cluster::Error as ClusterError,
};

/// 节点运行状态文件名。
pub use database::server::node::STATUS_FILE;
/// 运行状态文件版本。
pub use database::server::node::CONTROL_VERSION;
/// 运行中节点发布的运行状态元数据。
pub use database::server::node::RuntimeStatus;
/// 无 catalog 时使用的本地分片 id。
pub use database::cluster::catalog::node::LOCAL_SHARD_ID;
/// 运行状态文件读写。
pub use database::server::node::{new_runtime_status, read_runtime_status, write_runtime_status};
/// 停止标记与停止等待。
pub use database::server::node::{clear_runtime_artifacts, request_stop, stop_requested, wait_until_stopped};

/// 节点目录操作失败。
#[derive(Debug)]
pub enum NodeError {
    /// 显式给出的 catalog 不可用；不允许回退到单机路由。
    Catalog(CatalogLoadError),
    /// 分片文件或执行器装配失败。
    Storage(ClusterError),
    /// 目录所有权探测或获取失败。
    Lease(LeaseError),
    /// 运行状态文件读取失败。
    Status(RuntimeStatusError),
    /// 停止协调失败。
    Control(NodeControlError),
}

impl fmt::Display for NodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
            Self::Lease(error) => error.fmt(formatter),
            Self::Status(error) => error.fmt(formatter),
            Self::Control(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::Lease(error) => Some(error),
            Self::Status(error) => Some(error),
            Self::Control(error) => Some(error),
        }
    }
}

impl From<CatalogLoadError> for NodeError {
    fn from(error: CatalogLoadError) -> Self {
        Self::Catalog(error)
    }
}

impl From<ClusterError> for NodeError {
    fn from(error: ClusterError) -> Self {
        Self::Storage(error)
    }
}

impl From<LeaseError> for NodeError {
    fn from(error: LeaseError) -> Self {
        Self::Lease(error)
    }
}

impl From<RuntimeStatusError> for NodeError {
    fn from(error: RuntimeStatusError) -> Self {
        Self::Status(error)
    }
}

impl From<NodeControlError> for NodeError {
    fn from(error: NodeControlError) -> Self {
        Self::Control(error)
    }
}

/// [`NodeIdentity`] 的 JSON 投影:核心类型不携带 serde 派生。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeIdentityView {
    /// 集群 id。
    pub cluster_id: String,
    /// 节点 id。
    pub node_id: String,
}

impl NodeIdentityView {
    /// 把已校验的身份字段复制成其发布形状。
    pub fn of(identity: &NodeIdentity) -> Self {
        Self { cluster_id: identity.cluster_id().to_owned(), node_id: identity.node_id().to_owned() }
    }
}

/// 节点状态检查结果。
///
/// 本结构只被宿主逐字段读取并另建宿主对象,从不整体序列化。
#[derive(Debug, Clone)]
pub struct NodeStatusSnapshot {
    /// 节点目录。
    pub directory: String,
    /// 身份投影。
    pub identity: NodeIdentityView,
    /// 所有权状态(`held` / `available`)。
    pub ownership: String,
    /// 运行状态(仅持有所有权时读取)。
    pub runtime: Option<RuntimeStatus>,
    /// 目录健康描述。
    pub health: String,
    /// catalog 发布的 schema 版本。
    pub schema_revision: Option<u32>,
    /// 路由 epoch。
    pub routing_epoch: Option<u64>,
    /// 分片数量。
    pub shard_count: Option<usize>,
}

/// 初始化节点目录与本地分片存储。
pub fn init_node(data_dir: &Path, identity: NodeIdentity) -> Result<(), NodeError> {
    let _lease = NodeLease::acquire(data_dir, identity)?;
    open_executor(data_dir, None)?;
    Ok(())
}

/// 打开一个节点目录的本地键值执行器。
///
/// `catalog_path` 为 `None` 表示调用方显式选择单节点模式,只使用本地分片;为 `Some`
/// 时必须读到带已发布路由的 catalog,读取或校验失败都返回结构化错误,不回退。
pub fn open_executor(directory: &Path, catalog_path: Option<&Path>) -> Result<Arc<dyn KeyValueExecutor>, NodeError> {
    let facts = load_node_catalog(catalog_path)?;
    let routing = facts.routing.into_map();
    let shards = open_shards(directory, routing.shards())?;
    let executor = LocalExecutor::new(routing, shards, Vec::new())?;
    Ok(Arc::new(executor))
}

/// 检查节点所有权与可选运行状态。
///
/// 接受可选 catalog 路径,使健康快照反映 catalog 驱动的 schema 版本、路由
/// epoch 与分片数量,而不是每次宣称 `local-ownership-only`。
pub fn inspect_node(data_dir: &Path, catalog_path: Option<&Path>) -> Result<NodeStatusSnapshot, NodeError> {
    let state = NodeLease::inspect(data_dir)?;
    let runtime = if state.ownership == LocalOwnership::Held { read_runtime_status(data_dir)? } else { None };
    let facts = load_node_catalog(catalog_path)?;
    Ok(NodeStatusSnapshot {
        directory: state.directory.display().to_string(),
        identity: NodeIdentityView::of(&state.identity),
        ownership: match state.ownership {
            LocalOwnership::Held => "held".into(),
            LocalOwnership::Available => "available".into(),
        },
        runtime,
        health: health(catalog_path, &facts),
        schema_revision: facts.schema_revision,
        routing_epoch: Some(facts.routing.map().epoch().0),
        shard_count: Some(facts.routing.map().shards().len()),
    })
}

/// 目录健康描述:区分显式单节点与 catalog 驱动。
fn health(catalog_path: Option<&Path>, facts: &NodeCatalogFacts) -> String {
    let Some(path) = catalog_path
    else {
        return format!("local-only (shard={LOCAL_SHARD_ID})");
    };
    let epoch = facts.routing.map().epoch().0;
    let shards = facts.routing.map().shards().len();
    match facts.schema_revision {
        Some(revision) => format!("catalog-driven (path={}, schema_revision={revision}, routing_epoch={epoch}, shards={shards})", path.display()),
        None => format!("catalog-driven-schema-pending (path={}, routing_epoch={epoch}, shards={shards})", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use database::{
        cluster::catalog::{Catalog, catalog_path, node::CatalogLoadError},
        types::cluster::{ShardEpoch, ShardId},
    };
    use super::*;

    use database::cluster::control::NodeIdentity;

    struct TempDir(PathBuf);

    impl TempDir {
        fn create(label: &str) -> Self {
            let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock").as_nanos();
            let path = std::env::temp_dir().join(format!("yyds-node-{label}-{nonce}"));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_catalog_opens_the_local_split() {
        let dir = TempDir::create("local");
        open_executor(dir.path(), None).expect("local mode opens");
        assert!(dir.path().join(format!("{LOCAL_SHARD_ID}.yykv")).exists());
    }

    #[test]
    fn missing_catalog_path_is_refused_instead_of_falling_back() {
        let dir = TempDir::create("missing");
        let missing = catalog_path(dir.path().join("absent"));
        let error = open_executor(dir.path(), Some(&missing)).expect_err("a supplied but missing catalog must fail");
        let NodeError::Catalog(catalog) = error
        else {
            panic!("expected a structured catalog error, got {error}");
        };
        assert_eq!(catalog.kind(), "not_found");
        assert!(matches!(catalog, CatalogLoadError::NotFound { .. }));
        assert!(!dir.path().join(format!("{LOCAL_SHARD_ID}.yykv")).exists(), "a failed catalog open must not create a local shard");
    }

    #[test]
    fn corrupt_catalog_bytes_are_reported_as_a_parse_failure() {
        let dir = TempDir::create("corrupt");
        let path = catalog_path(dir.path().join("broken"));
        fs::write(&path, b"not a catalog").expect("write corrupt catalog");
        let error = open_executor(dir.path(), Some(&path)).expect_err("corrupt catalog must fail");
        let NodeError::Catalog(catalog) = error
        else {
            panic!("expected a structured catalog error, got {error}");
        };
        assert_eq!(catalog.kind(), "parse");
    }

    #[test]
    fn catalog_without_published_routing_is_refused() {
        let dir = TempDir::create("routing");
        let path = catalog_path(dir.path().join("empty"));
        Catalog::open(&path).expect("open empty catalog").flush().expect("flush empty catalog");
        let error = open_executor(dir.path(), Some(&path)).expect_err("a catalog without routing must fail");
        let NodeError::Catalog(catalog) = error
        else {
            panic!("expected a structured catalog error, got {error}");
        };
        assert_eq!(catalog.kind(), "routing_missing");
    }

    #[test]
    fn published_catalog_routing_drives_the_shard_split() {
        let dir = TempDir::create("published");
        let path = catalog_path(dir.path().join("cluster"));
        let mut catalog = Catalog::open(&path).expect("open catalog");
        catalog.publish_routing(ShardEpoch(7), vec![ShardId("shard-a".into()), ShardId("shard-b".into())]).expect("publish routing");
        catalog.flush().expect("flush catalog");

        let executor = open_executor(dir.path(), Some(&path)).expect("catalog mode opens");
        drop(executor);
        assert!(dir.path().join("shard-a.yykv").exists());
        assert!(dir.path().join("shard-b.yykv").exists());
        assert!(!dir.path().join(format!("{LOCAL_SHARD_ID}.yykv")).exists());

        let identity = NodeIdentity::new("cluster-a", "node-a").expect("identity");
        drop(NodeLease::acquire(dir.path(), identity).expect("lease"));

        let snapshot = inspect_node(dir.path(), Some(&path)).expect("inspect");
        assert_eq!(snapshot.routing_epoch, Some(7));
        assert_eq!(snapshot.shard_count, Some(2));
    }
}
