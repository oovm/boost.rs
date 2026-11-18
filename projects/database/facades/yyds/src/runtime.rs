//! 节点目录生命周期与运行状态持久化。

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use database::{
    cluster::{
        catalog::Catalog,
        control::{LocalOwnership, NodeIdentity, NodeLease},
        topology::ShardMap,
    },
    execution::cluster::{KeyValueExecutor, LocalExecutor},
    storage::cluster_kv::FileShard,
    types::cluster::{ShardEpoch, ShardId},
};
use serde::{Deserialize, Serialize};

/// 节点运行状态文件名。
pub const STATUS_FILE: &str = "node.status.json";
/// 运行状态文件版本。
pub const CONTROL_VERSION: u32 = 1;
/// 无 catalog 时使用的本地分片 id。
pub const LOCAL_SHARD_ID: &str = "local-0";

/// 运行中节点进程发布的运行状态元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeStatus {
    /// 控制文件版本。
    pub version: u32,
    /// 本次启动的代号,用于停止协调。
    pub generation: uuid::Uuid,
    /// 进程号。
    pub pid: u32,
    /// 启动时刻(Unix 毫秒)。
    pub started_at_unix_ms: u128,
    /// Redis 网关地址。
    pub redis_address: String,
    /// PostgreSQL 网关地址。
    pub pgsql_address: Option<String>,
    /// MySQL 网关地址。
    pub mysql_address: Option<String>,
    /// YY 线服务地址。
    pub wire_address: Option<String>,
}

/// [`NodeIdentity`] 的 JSON 投影:核心类型不携带 serde 派生。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// 从可选 `.yyds` catalog 文件读出 schema 版本与路由事实。
///
/// catalog 缺失、损坏或没有发布路由时回退到单 `local-0` 分片,并在 stderr
/// 明确告警,不做静默硬编码。
fn load_catalog_facts(catalog_path: Option<&Path>) -> CatalogFacts {
    let Some(path) = catalog_path
    else {
        return CatalogFacts::fallback(LOCAL_SHARD_ID);
    };
    if !path.exists() {
        eprintln!("[yyds] WARNING: catalog file {} not found falling back to local-only shard", path.display());
        return CatalogFacts::fallback(LOCAL_SHARD_ID);
    }
    match Catalog::open(path) {
        Ok(catalog) => {
            let schema_revision = catalog.schema().map(|schema| schema.version);
            let (routing, shards) = match catalog.routing() {
                Ok(Some(map)) => (Some(map.epoch()), map.shards().to_vec()),
                Ok(None) => {
                    eprintln!("[yyds] WARNING: catalog has no published routing epoch falling back to local-only shard");
                    return CatalogFacts::fallback(LOCAL_SHARD_ID);
                }
                Err(error) => {
                    eprintln!("[yyds] WARNING: catalog routing load failed ({error}) falling back to local-only shard");
                    return CatalogFacts::fallback(LOCAL_SHARD_ID);
                }
            };
            CatalogFacts { schema_revision, routing, shards, source: CatalogSource::File(path.to_path_buf()) }
        }
        Err(error) => {
            eprintln!("[yyds] WARNING: catalog open failed ({error}) falling back to local-only shard");
            CatalogFacts::fallback(LOCAL_SHARD_ID)
        }
    }
}

/// 一个节点目录解析出的路由与分片事实。
#[derive(Debug, Clone)]
struct CatalogFacts {
    schema_revision: Option<u32>,
    routing: Option<ShardEpoch>,
    shards: Vec<ShardId>,
    source: CatalogSource,
}

/// 事实来源,用于生成健康描述。
#[derive(Debug, Clone)]
enum CatalogSource {
    /// 来自该路径的 catalog 文件。
    File(PathBuf),
    /// catalog 缺失或无效时的本地回退。
    Fallback,
}

impl CatalogFacts {
    fn fallback(local_shard_id: &str) -> Self {
        Self {
            schema_revision: None,
            routing: Some(ShardEpoch(1)),
            shards: vec![ShardId(local_shard_id.to_string())],
            source: CatalogSource::Fallback,
        }
    }

    fn health(&self) -> String {
        match (&self.source, self.schema_revision, self.routing) {
            (CatalogSource::File(path), Some(rev), Some(epoch)) => {
                format!(
                    "catalog-driven (path={}, schema_revision={}, routing_epoch={}, shards={})",
                    path.display(),
                    rev,
                    epoch.0,
                    self.shards.len()
                )
            }
            (CatalogSource::File(path), None, Some(epoch)) => {
                format!(
                    "catalog-driven-schema-pending (path={}, routing_epoch={}, shards={})",
                    path.display(),
                    epoch.0,
                    self.shards.len()
                )
            }
            (CatalogSource::File(path), Some(rev), None) => {
                format!("catalog-driven-routing-pending (path={}, schema_revision={})", path.display(), rev)
            }
            (CatalogSource::File(path), None, None) => format!("catalog-driven-empty (path={})", path.display()),
            (CatalogSource::Fallback, _, _) => "local-fallback".into(),
        }
    }
}

/// 初始化节点目录与本地分片存储。
pub fn init_node(data_dir: &Path, identity: NodeIdentity) -> Result<(), String> {
    let _lease = NodeLease::acquire(data_dir, identity).map_err(|error| error.to_string())?;
    open_executor(data_dir, None)?;
    Ok(())
}

/// 打开一个节点目录的本地键值执行器。
///
/// `catalog_path` 指向已发布的 `.yyds` 文件时使用 catalog 的路由 epoch 与分片
/// 布局;catalog 缺失或无效时回退到单个 `local-0` 分片并在 stderr 告警。
pub fn open_executor(directory: &Path, catalog_path: Option<&Path>) -> Result<Arc<dyn KeyValueExecutor>, String> {
    let facts = load_catalog_facts(catalog_path);
    let epoch = facts.routing.ok_or_else(|| String::from("catalog routing failed unexpectedly"))?;
    let routing = ShardMap::new(epoch, facts.shards.clone()).map_err(|error| format!("routing: {error:?}"))?;

    let mut shards = std::collections::HashMap::new();
    for shard_id in &facts.shards {
        let shard_path = directory.join(format!("{}.yykv", shard_id.0));
        let shard = FileShard::open(&shard_path)
            .map_err(|error| format!("failed to open shard {} at {}: {error}", shard_id.0, shard_path.display()))?;
        shards.insert(shard_id.clone(), shard);
    }
    let executor = LocalExecutor::new(routing, shards, Vec::new()).map_err(|error| error.to_string())?;
    Ok(Arc::new(executor))
}

/// 检查节点所有权与可选运行状态。
///
/// 接受可选 catalog 路径,使健康快照反映 catalog 驱动的 schema 版本、路由
/// epoch 与分片数量,而不是每次宣称 `local-ownership-only`。
pub fn inspect_node(data_dir: &Path, catalog_path: Option<&Path>) -> Result<NodeStatusSnapshot, String> {
    let state = NodeLease::inspect(data_dir).map_err(|error| error.to_string())?;
    let runtime = if state.ownership == LocalOwnership::Held { read_runtime_status(data_dir)? } else { None };
    let facts = load_catalog_facts(catalog_path);
    Ok(NodeStatusSnapshot {
        directory: state.directory.display().to_string(),
        identity: NodeIdentityView::of(&state.identity),
        ownership: match state.ownership {
            LocalOwnership::Held => "held".into(),
            LocalOwnership::Available => "available".into(),
        },
        runtime,
        health: facts.health(),
        schema_revision: facts.schema_revision,
        routing_epoch: facts.routing.map(|epoch| epoch.0),
        shard_count: Some(facts.shards.len()),
    })
}

/// 原子写入运行状态文件。
pub fn write_runtime_status(directory: &Path, runtime: &RuntimeStatus) -> Result<(), String> {
    let bytes = serde_json::to_vec(runtime).map_err(|error| error.to_string())?;
    let temporary = directory.join("node.status.json.tmp");
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(temporary, directory.join(STATUS_FILE)).map_err(|error| error.to_string())
}

/// 读取并校验运行状态文件。
pub fn read_runtime_status(directory: &Path) -> Result<Option<RuntimeStatus>, String> {
    match File::open(directory.join(STATUS_FILE)) {
        Ok(input) => {
            if !input.metadata().map_err(|error| error.to_string())?.is_file() {
                return Err("runtime status must be a regular file".into());
            }
            let mut bytes = Vec::new();
            input.take(4097).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
            if bytes.len() > 4096 {
                return Err("runtime status exceeds byte limit".into());
            }
            let status: RuntimeStatus = serde_json::from_slice(&bytes).map_err(|error| format!("corrupt runtime status: {error}"))?;
            if status.version != CONTROL_VERSION || status.generation.is_nil() {
                return Err("unsupported or invalid runtime status".into());
            }
            for address in
                [Some(status.redis_address.as_str()), status.pgsql_address.as_deref(), status.mysql_address.as_deref()]
                    .into_iter()
                    .flatten()
            {
                if !address.starts_with("127.0.0.1:") && !address.starts_with("[::1]:") {
                    return Err("unsupported or invalid runtime status".into());
                }
            }
            Ok(Some(status))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// 为给定运行代号建立停止标记。
pub fn request_stop(directory: &Path, generation: uuid::Uuid) -> Result<(), String> {
    let path = stop_path(directory, generation);
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// 等待节点目录重新可用或超时。
pub fn wait_until_stopped(directory: &Path, generation: uuid::Uuid, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match NodeLease::inspect(directory) {
            Ok(state) if state.ownership == LocalOwnership::Available => return Ok(()),
            Err(error) => return Err(error.to_string()),
            _ if Instant::now() >= deadline => return Err("timed out waiting for node stop".into()),
            _ => {
                if let Some(current) = read_runtime_status(directory)? {
                    if current.generation != generation {
                        return Err("node generation changed while stopping".into());
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 清理运行状态与停止标记文件。
pub fn clear_runtime_artifacts(directory: &Path, generation: uuid::Uuid) {
    let _ = fs::remove_file(directory.join(STATUS_FILE));
    let _ = fs::remove_file(stop_path(directory, generation));
}

/// 该运行代号的停止标记是否存在。
pub fn stop_requested(directory: &Path, generation: uuid::Uuid) -> bool {
    stop_path(directory, generation).exists()
}

/// 为启动中的进程构造运行状态快照。
pub fn new_runtime_status(
    redis_address: String,
    pgsql_address: Option<String>,
    mysql_address: Option<String>,
    wire_address: Option<String>,
) -> RuntimeStatus {
    RuntimeStatus {
        version: CONTROL_VERSION,
        generation: uuid::Uuid::new_v4(),
        pid: std::process::id(),
        started_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_millis()).unwrap_or(0),
        redis_address,
        pgsql_address,
        mysql_address,
        wire_address,
    }
}

fn stop_path(directory: &Path, generation: uuid::Uuid) -> PathBuf {
    directory.join(format!("node.stop.{generation}"))
}
