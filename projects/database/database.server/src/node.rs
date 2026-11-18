//! 节点运行时协调：运行状态文件、运行代号与停止标记。
//!
//! 运行状态文件描述「这个节点目录当前由哪个进程、哪次启动持有」，停止标记把
//! 「请求停止」从信号转成节点目录里的可观察事实。健康快照的组装、catalog 选择
//! 与执行器装配不属于本模块。

use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use database_cluster::control::{LeaseError, LocalOwnership, NodeLease};
use serde::{Deserialize, Serialize};

/// 节点运行状态文件名。
pub const STATUS_FILE: &str = "node.status.json";
/// 运行状态文件版本。
pub const CONTROL_VERSION: u32 = 1;
/// 运行状态文件最大字节数。
const MAX_STATUS_BYTES: u64 = 4096;

/// 运行状态文件不可用。
#[derive(Debug)]
pub enum RuntimeStatusError {
    /// 读取或写入运行状态文件的 I/O 失败。
    Io(io::Error),
    /// 文件内容超出边界或与当前控制版本不符。
    Corrupt(&'static str),
}

impl fmt::Display for RuntimeStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "node status I/O error: {error}"),
            Self::Corrupt(detail) => write!(formatter, "corrupt runtime status: {detail}"),
        }
    }
}

impl std::error::Error for RuntimeStatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corrupt(_) => None,
        }
    }
}

impl From<io::Error> for RuntimeStatusError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 节点停止协调失败。
#[derive(Debug)]
pub enum NodeControlError {
    /// 目录所有权探测失败。
    Lease(LeaseError),
    /// 读取运行状态文件失败。
    Status(RuntimeStatusError),
    /// 等待停止超时。
    Timeout {
        /// 调用方给出的等待上限（毫秒）。
        timeout_ms: u64,
    },
    /// 停止等待期间运行代号已更换。
    GenerationChanged,
}

impl fmt::Display for NodeControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lease(error) => error.fmt(formatter),
            Self::Status(error) => error.fmt(formatter),
            Self::Timeout { timeout_ms } => write!(formatter, "timed out waiting {timeout_ms} ms for node stop"),
            Self::GenerationChanged => formatter.write_str("node generation changed while stopping"),
        }
    }
}

impl std::error::Error for NodeControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lease(error) => Some(error),
            Self::Status(error) => Some(error),
            Self::Timeout { .. } | Self::GenerationChanged => None,
        }
    }
}

impl From<LeaseError> for NodeControlError {
    fn from(error: LeaseError) -> Self {
        Self::Lease(error)
    }
}

impl From<RuntimeStatusError> for NodeControlError {
    fn from(error: RuntimeStatusError) -> Self {
        Self::Status(error)
    }
}

/// 运行中节点进程发布的运行状态元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeStatus {
    /// 控制文件版本。
    pub version: u32,
    /// 本次启动的代号，用于停止协调。
    pub generation: uuid::Uuid,
    /// 进程号。
    pub pid: u32,
    /// 启动时刻（Unix 毫秒）。
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

/// 原子写入运行状态文件。
pub fn write_runtime_status(directory: &Path, runtime: &RuntimeStatus) -> Result<(), RuntimeStatusError> {
    let bytes = serde_json::to_vec(runtime).map_err(|_| RuntimeStatusError::Corrupt("runtime status is not serializable"))?;
    let temporary = directory.join("node.status.json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, directory.join(STATUS_FILE))?;
    Ok(())
}

/// 读取并校验运行状态文件。
pub fn read_runtime_status(directory: &Path) -> Result<Option<RuntimeStatus>, RuntimeStatusError> {
    let mut input = match fs::File::open(directory.join(STATUS_FILE)) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RuntimeStatusError::Io(error)),
    };
    if !input.metadata()?.is_file() {
        return Err(RuntimeStatusError::Corrupt("runtime status must be a regular file"));
    }
    let mut bytes = Vec::new();
    input.take(MAX_STATUS_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATUS_BYTES {
        return Err(RuntimeStatusError::Corrupt("runtime status exceeds byte limit"));
    }
    let status: RuntimeStatus =
        serde_json::from_slice(&bytes).map_err(|_| RuntimeStatusError::Corrupt("runtime status is not valid JSON"))?;
    if status.version != CONTROL_VERSION || status.generation.is_nil() {
        return Err(RuntimeStatusError::Corrupt("unsupported or invalid runtime status"));
    }
    for address in [Some(status.redis_address.as_str()), status.pgsql_address.as_deref(), status.mysql_address.as_deref()].into_iter().flatten() {
        if !address.starts_with("127.0.0.1:") && !address.starts_with("[::1]:") {
            return Err(RuntimeStatusError::Corrupt("unsupported or invalid runtime status"));
        }
    }
    Ok(Some(status))
}

/// 为给定运行代号建立停止标记。
pub fn request_stop(directory: &Path, generation: uuid::Uuid) -> Result<(), RuntimeStatusError> {
    match fs::OpenOptions::new().write(true).create_new(true).open(stop_path(directory, generation)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(RuntimeStatusError::Io(error)),
    }
}

/// 等待节点目录重新可用或超时。
///
/// 等待期间若运行代号被换成另一次启动，说明本次停止请求已经过期，立即失败。
pub fn wait_until_stopped(directory: &Path, generation: uuid::Uuid, timeout: Duration) -> Result<(), NodeControlError> {
    let deadline = Instant::now() + timeout;
    loop {
        match NodeLease::inspect(directory) {
            Ok(state) if state.ownership == LocalOwnership::Available => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(NodeControlError::Lease(error)),
        }
        if Instant::now() >= deadline {
            return Err(NodeControlError::Timeout { timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX) });
        }
        if let Some(current) = read_runtime_status(directory)? {
            if current.generation != generation {
                return Err(NodeControlError::GenerationChanged);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
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
