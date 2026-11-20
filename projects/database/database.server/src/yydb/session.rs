//! 线服务与执行后端之间的会话端口。
//!
//! 线服务只负责帧收发、鉴权门与权限门;命令语义由 [`ServeBackend`] 的实现拥有。
//! 嵌入式引擎由 `database.execution::engine::Connection` 直接实现本 trait,
//! 分片节点(如 YYDS)提供基于 `KeyValueExecutor` 的实现,两者共用同一套 wire 服务,
//! 不各自复制 serve 循环或鉴权状态机。

use std::{collections::BTreeMap, net::TcpStream};

use database_execution::engine::{Connection, PlanOutcome};
use database_query::local::{PreparedStatement, QueryRow};
use database_types::{Error, Result, SchemaVersion, Value};
use database_udf::HostMicroDefinition;

use super::host_wire::TcpWireHostAdapter;

/// 一条连接背后的执行后端。
///
/// 除 `Info` / schema / KV 五条基础命令外,其余方法默认以
/// [`Error::Unsupported`] 拒绝,由具备该能力的后端覆盖。
#[allow(unused_variables)]
pub trait ServeBackend: Send {
    /// `Info` 回复正文。
    fn info(&self) -> Result<Vec<u8>>;

    /// 当前发布的 schema。
    fn schema(&self) -> Result<Option<SchemaVersion>>;

    /// 把 schema 收敛到目标文档;`version` 是客户端声明的版本。
    fn ensure_schema(&self, version: u32, document: &str) -> Result<()>;

    /// 读一个 key。
    fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// 写一个 key。
    fn kv_put(&self, key: String, value: Vec<u8>) -> Result<()>;

    /// 注册会话级宿主微函数。
    fn register_host_micro(&self, definition: HostMicroDefinition) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve host micro registration"))
    }

    /// 调用已注册的标量 UDF。
    fn call_scalar_version(&self, name: &str, version: u32, args: &[Value]) -> Result<Value> {
        Err(Error::Unsupported("this backend does not serve scalar UDF calls"))
    }

    /// 预编译一段查询源。
    fn prepare(&self, source: &str) -> Result<PreparedStatement> {
        Err(Error::Unsupported("this backend does not serve prepared statements"))
    }

    /// 执行已预编译的语句。
    fn query_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        Err(Error::Unsupported("this backend does not serve prepared statements"))
    }

    /// 执行已预编译的写语句(INSERT / UPDATE / DELETE 或 VOS 写程序)。
    ///
    /// 与 [`Self::query_prepared`] 分开,避免把写计划送进只驱动读管线的路径。
    fn execute_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        Err(Error::Unsupported("this backend does not serve prepared writes"))
    }

    /// 打开显式事务。
    fn begin(&self) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve transactions"))
    }

    /// 提交显式事务。
    fn commit(&self) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve transactions"))
    }

    /// 回滚显式事务。
    fn rollback(&self) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve transactions"))
    }

    /// 建立保存点。
    fn savepoint(&self, name: &str) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve savepoints"))
    }

    /// 回滚到保存点。
    fn rollback_to_savepoint(&self, name: &str) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve savepoints"))
    }

    /// 释放保存点。
    fn release_savepoint(&self, name: &str) -> Result<()> {
        Err(Error::Unsupported("this backend does not serve savepoints"))
    }

    /// 把连接的传输层交给后端,供 `MicroHostInvoke` 反向调用宿主微函数。
    fn attach_transport(&self, stream: &TcpStream) -> Result<()> {
        Ok(())
    }

    /// 运行时性能快照(JSON),供 `PerfReport` 持续改进路径采集卡点。
    fn perf_report(&self) -> Result<String> {
        Err(Error::Unsupported("this backend does not serve perf reports"))
    }
}

/// 为新连接建立执行后端。
pub trait BackendFactory: Send + Sync {
    /// 为一条新连接打开后端;`peer_is_loopback` 供实现决定开发态资源。
    fn open(&self, peer_is_loopback: bool) -> Box<dyn ServeBackend>;
}

impl ServeBackend for Connection {
    fn info(&self) -> Result<Vec<u8>> {
        let path = self.path().map(|path| path.display().to_string()).unwrap_or_else(|| "<memory>".to_owned());
        let schema_line = match Connection::schema(self)? {
            Some(schema) => format!("schema.version={}", schema.version),
            None => "schema.version=".to_owned(),
        };
        let text = format!("path={path}\n{schema_line}\nrecords={}\njournal_mode={}\n", self.record_count()?, self.journal_mode().as_str());
        Ok(text.into_bytes())
    }

    fn schema(&self) -> Result<Option<SchemaVersion>> {
        Connection::schema(self)
    }

    fn ensure_schema(&self, _version: u32, document: &str) -> Result<()> {
        Connection::ensure_schema(self, document)
    }

    fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Connection::get(self, key)
    }

    fn kv_put(&self, key: String, value: Vec<u8>) -> Result<()> {
        Connection::put(self, key, value)
    }

    fn register_host_micro(&self, definition: HostMicroDefinition) -> Result<()> {
        Connection::register_host_micro(self, definition)
    }

    fn call_scalar_version(&self, name: &str, version: u32, args: &[Value]) -> Result<Value> {
        Connection::call_scalar_version(self, name, version, args)
    }

    fn prepare(&self, source: &str) -> Result<PreparedStatement> {
        Connection::prepare_vos(self, source)
    }

    fn query_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        Connection::query_prepared(self, statement, params)
    }

    fn execute_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        let values = database_query::local::bind_named(statement, params)?;
        match Connection::execute_plan(self, statement, &values)? {
            PlanOutcome::Affected(_) => Ok(Vec::new()),
            PlanOutcome::Rows(rows) => Ok(rows),
        }
    }

    fn begin(&self) -> Result<()> {
        Connection::begin(self)
    }

    fn commit(&self) -> Result<()> {
        Connection::commit(self)
    }

    fn rollback(&self) -> Result<()> {
        Connection::rollback(self)
    }

    fn savepoint(&self, name: &str) -> Result<()> {
        Connection::savepoint(self, name)
    }

    fn rollback_to_savepoint(&self, name: &str) -> Result<()> {
        Connection::rollback_to_savepoint(self, name)
    }

    fn release_savepoint(&self, name: &str) -> Result<()> {
        Connection::release_savepoint(self, name)
    }

    fn attach_transport(&self, stream: &TcpStream) -> Result<()> {
        let cloned = stream.try_clone().map_err(Error::Io)?;
        self.set_host_adapter(std::sync::Arc::new(TcpWireHostAdapter::new(cloned)));
        Ok(())
    }
}
