//! 线服务与执行后端之间的会话端口。
//!
//! 线服务只负责帧收发、鉴权门与权限门;命令语义由 [`ServeBackend`] 的实现拥有。
//! 嵌入式引擎由 `database.execution::engine::Connection` 直接实现本 trait,
//! 分片节点(如 YYDS)提供基于 `KeyValueExecutor` 的实现,两者共用同一套 wire 服务,
//! 不各自复制 serve 循环或鉴权状态机。

use std::{collections::BTreeMap, net::TcpStream};

use database_execution::engine::{Connection, PlanOutcome};
use database_formats::yy::row::{NamedFieldLayout, NamedValueLayout, ValueLayout};
use database_protocol_yy::{WireNamedDirectory, WireNamedField, WireNamedLayout, WireNamedVariant, WireValueLayout};
use database_query::local::{PreparedStatement, QueryRow};
use database_types::{Error, Result, SchemaVersion, Value};
use database_udf::HostMicroDefinition;
use schema_types::TypeContractKind;

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

    /// 当前运行目录的具名 wire 投影。
    ///
    /// `Some` 表示本后端的 schema 声明了 `class` / `structure` / `unite` /
    /// `enums` / `flags`，prepared 结果必须按具名投影发送；`None` 保持普通行集。
    /// 判定只来自运行目录，不看结果值或字段名。
    fn wire_named_directory(&self) -> Result<Option<WireNamedDirectory>> {
        Ok(None)
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

    /// 显式关闭会话：回滚未提交事务并释放会话持有的写租约。
    ///
    /// 线服务在回复 `SessionCloseOk` 后关闭连接；本方法保证释放不依赖
    /// 后端对象的析构时机,后继会话可以立即取得同一租约。
    fn close(&self) -> Result<()> {
        Ok(())
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

    fn wire_named_directory(&self) -> Result<Option<WireNamedDirectory>> {
        let Some(contract) = Connection::resolved_contract(self)? else {
            return Ok(None);
        };
        named_wire_directory(&contract)
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

    fn close(&self) -> Result<()> {
        // An open transaction must not outlive the session; rollback without an
        // open transaction is reported by the engine, so the result is ignored.
        let _ = Connection::rollback(self);
        Ok(())
    }
}

/// 从运行目录构造 prepared 结果的具名 wire 投影。
///
/// 只有 schema 真的声明了具名类型时才返回 `Some`；`enums` / `flags` 本身是具名
/// 数值，不产生具名记录声明，但同样要求走具名投影，避免以后给它们加线编码时
/// 还要改判定。
fn named_wire_directory(contract: &schema_types::ResolvedContract) -> Result<Option<WireNamedDirectory>> {
    let declares_named = contract.types.iter().any(|item| {
        matches!(
            item.kind,
            TypeContractKind::Class
                | TypeContractKind::Structure
                | TypeContractKind::Unite
                | TypeContractKind::Enums
                | TypeContractKind::Flags
        )
    });
    if !declares_named {
        return Ok(None);
    }
    let stored = database_execution::engine::catalog::row_schema_directory(contract)?;
    let mut columns = BTreeMap::new();
    for item in contract.types.iter().filter(|item| item.kind == TypeContractKind::Table) {
        for field in &item.fields {
            let Some(layout) = stored.column(&field.canonical_name) else {
                continue;
            };
            if matches!(layout, ValueLayout::Dynamic) {
                continue;
            }
            columns.insert(field.canonical_name.clone(), wire_layout(layout));
        }
    }
    let mut declarations = BTreeMap::new();
    for item in &contract.types {
        let Some(declaration) = stored.named(item.type_id) else {
            continue;
        };
        declarations.insert(item.type_id, wire_declaration(declaration));
    }
    Ok(Some(WireNamedDirectory::new(columns, declarations)))
}

fn wire_layout(layout: &ValueLayout) -> WireValueLayout {
    match layout {
        ValueLayout::Dynamic => WireValueLayout::Dynamic,
        ValueLayout::List(inner) => WireValueLayout::List(Box::new(wire_layout(inner))),
        ValueLayout::Named(schema_id) => WireValueLayout::Named(*schema_id),
    }
}

fn wire_declaration(declaration: &NamedValueLayout) -> WireNamedLayout {
    match declaration {
        NamedValueLayout::Class { schema_id, fields } => WireNamedLayout::Class { schema_id: *schema_id, fields: wire_fields(fields) },
        NamedValueLayout::Structure { schema_id, revision, fields } => {
            WireNamedLayout::Structure { schema_id: *schema_id, revision: *revision, fields: wire_fields(fields) }
        }
        NamedValueLayout::Unite { schema_id, revision, variants } => WireNamedLayout::Unite {
            schema_id: *schema_id,
            revision: *revision,
            variants: variants
                .iter()
                .map(|variant| WireNamedVariant { discriminant: variant.discriminant, fields: wire_fields(&variant.fields) })
                .collect(),
        },
    }
}

fn wire_fields(fields: &[NamedFieldLayout]) -> Vec<WireNamedField> {
    fields.iter().map(|field| WireNamedField { name: field.name.clone(), ty: wire_layout(&field.ty) }).collect()
}
