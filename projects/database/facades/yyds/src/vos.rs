//! VOS 执行后端：把分片键值执行器变成 YY 线服务的 [`ServeBackend`]。
//!
//! 读路径把分片上的行记录扫进记录表，交给 `database.execution::local` 的算子
//! 执行器；写路径在记录表上跑同一个 VOS 写程序，再把差分交给
//! [`KeyValueExecutor::commit_batch`]，由存储的单分片原子提交边界落盘。
//!
//! # 并发与原子性
//!
//! 一个节点内的写与事务由 [`WriteGate`] 串行化：非事务写的「扫描 → 应用 →
//! 提交」在同一段临界区内完成，事务则在整个 `BEGIN`…`COMMIT` 期间持有临界区。
//! 事务的提交仍走 `commit_batch`，每个被改动的键带上 `ValueEquals` 前像条件，
//! 所以并发进程越过临界区写入时会以 OCC 失败而不是静默覆盖。
//!
//! 原子域仍是**单分片**：`commit_batch` 跨分片会被执行器拒绝，不会被伪装成
//! 分布式事务。ZZZ 的每个节点只挂一个分片，因此其事务语义完整；多分片部署
//! 需要显式失败，而不是近似。

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use database::{
    execution::{
        cluster::{BufferedCommit, CatalogSchemaService, CommitCondition, KeyValueExecutor},
        engine::bytes::Placement,
        local::execute_ops,
    },
    formats::yy::row::{ObjectResolver, with_object_resolver},
    harness::{Hotspot, report_json},
    query::{
        cluster_kv::{KeyValueAction, KeyValueCommand},
        local::{
            PreparedStatement, QueryKind, QueryRow, bind_named, constraints, fingerprint, lower_write_program, parse_vos_write, prepare,
            resolved_contract_from_document,
        },
    },
    server::yydb::{BackendFactory, ServeBackend},
    types::{Error, Result, SchemaVersion, TableUniqueConstraint, Value, cluster::Namespace},
};

/// 线协议没有命名空间概念，所有 VOS 记录落在固定命名空间。
pub const VOS_NAMESPACE: &str = "default";

/// 行记录的物理键前缀（与嵌入式 `.yydb` 完全一致）。
const ROW_PREFIX: &[u8] = b"__yydb/v0/row/";

/// 行记录键前缀的文本形态，用于按表拼扫描前缀。
const ROW_PREFIX_TEXT: &str = "__yydb/v0/row/";

/// 单文件 `.yydb` 内嵌对象分块的键前缀。
const OBJECT_PREFIX: &str = "__yydb/v0/object/";

/// 单文件 `.yydb` 的 CAS 分块大小（与对象写入端一致）。
const OBJECT_CHUNK_BYTES: usize = 2048;

// ─── 写闸门 ───────────────────────────────────────────────────────────────

/// 节点级写闸门：非事务写与显式事务互斥，保证「扫描 → 提交」不被打断。
#[derive(Debug, Default)]
struct WriteGate {
    locked: Mutex<bool>,
    ready: Condvar,
}

impl WriteGate {
    fn acquire(&self) {
        let mut locked = self.lock();
        while *locked {
            locked = self.ready.wait(locked).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *locked = true;
    }

    fn release(&self) {
        let mut locked = self.lock();
        *locked = false;
        self.ready.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.locked.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 非事务写持有的临界区租约；丢弃即释放。
struct GateGuard<'a>(&'a WriteGate);

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

// ─── 对象解析 ─────────────────────────────────────────────────────────────

/// 从节点分片读取内嵌 CAS 分块，供行解码解析 out-of-line 引用。
///
/// 对象按内容寻址且不可变，因此事务内也可以安全地按已提交分片解析。
struct ShardObjects {
    executor: Arc<dyn KeyValueExecutor>,
}

impl ObjectResolver for ShardObjects {
    fn resolve(&self, object: &database::types::ObjectRef) -> Result<Vec<u8>> {
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let hash = object.hash_hex();
        let total = object.size as usize;
        let mut payload = Vec::with_capacity(total);
        let mut index = 0usize;
        while payload.len() < total {
            let key = format!("{OBJECT_PREFIX}{hash}/{index:06}");
            let command = KeyValueCommand { namespace: namespace.clone(), key: key.into_bytes(), action: KeyValueAction::Get };
            match self.executor.execute(None, command).map_err(Error::from)? {
                database::query::cluster_kv::KeyValueResult::Get(Some(bytes)) => payload.extend_from_slice(&bytes),
                _ => {
                    return Err(Error::Corrupt("YYDS node is missing an object chunk referenced by a row"));
                }
            }
            if index > total / OBJECT_CHUNK_BYTES + 1 {
                return Err(Error::Corrupt("YYDS object chunk chain does not terminate"));
            }
            index += 1;
        }
        Ok(payload)
    }
}

// ─── 节点与后端 ───────────────────────────────────────────────────────────

/// 一个 YYDS 节点对 VOS 暴露的共享状态。
pub struct VosNode {
    executor: Arc<dyn KeyValueExecutor>,
    catalog: Option<CatalogSchemaService>,
    objects: Arc<dyn ObjectResolver>,
    unique: constraints::UniqueIndex,
    gate: WriteGate,
    perf: Hotspot,
}

impl std::fmt::Debug for VosNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("VosNode").field("namespace", &VOS_NAMESPACE).field("catalog", &self.catalog.is_some()).finish()
    }
}

impl VosNode {
    /// 用分片执行器与可选 catalog schema 服务组装节点。
    pub fn new(executor: Arc<dyn KeyValueExecutor>, catalog: Option<CatalogSchemaService>) -> Self {
        let objects: Arc<dyn ObjectResolver> = Arc::new(ShardObjects { executor: Arc::clone(&executor) });
        Self { executor, catalog, objects, unique: constraints::UniqueIndex::new(), gate: WriteGate::default(), perf: Hotspot::new() }
    }

    /// 当前已发布的 schema。
    pub fn schema(&self) -> Option<SchemaVersion> {
        self.catalog.as_ref()?.schema().map(|schema| SchemaVersion { version: schema.version, document: schema.document })
    }

    /// 收敛节点 schema；没有 catalog 时显式拒绝。
    pub fn ensure_schema(&self, version: u32, document: &str) -> Result<()> {
        let Some(catalog) = &self.catalog
        else {
            return Err(Error::Unsupported("this YYDS node has no catalog; schema changes are not served"));
        };
        catalog.ensure_schema(version, document).map_err(Error::from)
    }

    /// 运行时性能记录器（宿主诊断与持续调优的公共出口）。
    pub fn hotspot(&self) -> &Hotspot {
        &self.perf
    }

    fn document(&self) -> Result<(u32, String)> {
        self.schema()
            .map(|schema| (schema.version, schema.document))
            .ok_or_else(|| Error::Schema { message: "YYDS node has no published schema; ensure the schema before running VOS".to_owned() })
    }

    fn load_rows(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let pairs = self.executor.execute_scan(None, &namespace, ROW_PREFIX, None, None, 0).map_err(Error::from)?;
        collect_rows(pairs)
    }

    /// 只扫描语句引用到的表，避免每次读都把整库搬进内存。
    ///
    /// `tables` 为空（离线 prepared、解析不到引用）时退回整库扫描，宁可慢
    /// 也不漏。
    fn load_rows_for(&self, tables: &[String]) -> Result<BTreeMap<String, Vec<u8>>> {
        if tables.is_empty() {
            return self.load_rows();
        }
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let mut records = BTreeMap::new();
        let mut seen: Vec<&str> = Vec::with_capacity(tables.len());
        for table in tables {
            if seen.contains(&table.as_str()) {
                continue;
            }
            seen.push(table);
            let prefix = format!("{ROW_PREFIX_TEXT}{table}/");
            let pairs = self.executor.execute_scan(None, &namespace, prefix.as_bytes(), None, None, 0).map_err(Error::from)?;
            records.extend(collect_rows(pairs)?);
        }
        Ok(records)
    }

    fn commit_diff(&self, base: &BTreeMap<String, Vec<u8>>, records: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let mut batch = Vec::new();
        for (key, value) in records {
            if base.get(key) == Some(value) {
                continue;
            }
            batch.push(BufferedCommit {
                command: KeyValueCommand { namespace: namespace.clone(), key: key.as_bytes().to_vec(), action: KeyValueAction::Put(value.clone()) },
                condition: Some(CommitCondition::ValueEquals(base.get(key).cloned())),
            });
        }
        for (key, previous) in base {
            if records.contains_key(key) {
                continue;
            }
            batch.push(BufferedCommit {
                command: KeyValueCommand { namespace: namespace.clone(), key: key.as_bytes().to_vec(), action: KeyValueAction::Delete },
                condition: Some(CommitCondition::ValueEquals(Some(previous.clone()))),
            });
        }
        if batch.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        self.executor.commit_batch(None, batch).map_err(Error::from)?;
        self.perf.record("yyds.node.commit", started.elapsed());
        Ok(())
    }
}

/// 把分片扫描结果转成记录表（键必须是 UTF-8 记录键）。
fn collect_rows(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut records = BTreeMap::new();
    for (key, value) in pairs {
        let key = String::from_utf8(key).map_err(|_| Error::Corrupt("YYDS record key is not utf-8"))?;
        records.insert(key, value);
    }
    Ok(records)
}

/// 一条连接上的事务快照。
struct TxnState {
    base: BTreeMap<String, Vec<u8>>,
    records: BTreeMap<String, Vec<u8>>,
    savepoints: Vec<(String, BTreeMap<String, Vec<u8>>)>,
}

/// 一条连接的 VOS 后端。
pub struct VosBackend {
    node: Arc<VosNode>,
    txn: Mutex<Option<TxnState>>,
    holds_gate: Mutex<bool>,
    closed: AtomicBool,
}

impl VosBackend {
    /// 用节点共享状态建立一条连接的后端。
    pub fn new(node: Arc<VosNode>) -> Self {
        Self { node, txn: Mutex::new(None), holds_gate: Mutex::new(false), closed: AtomicBool::new(false) }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) { Err(Error::SessionClosed) } else { Ok(()) }
    }

    /// 显式释放本会话：丢弃未提交事务并交还写闸门。
    ///
    /// 幂等；`close` 帧与对象析构都会走同一入口，所以后继会话不必等待
    /// 旧后端被 GC。
    fn release_lease(&self) {
        self.closed.store(true, Ordering::Release);
        self.txn().take();
        self.exit_gate();
    }

    fn txn(&self) -> MutexGuard<'_, Option<TxnState>> {
        self.txn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enter_gate(&self) {
        self.node.gate.acquire();
        *self.holds_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    fn exit_gate(&self) {
        let mut holds = self.holds_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *holds {
            *holds = false;
            self.node.gate.release();
        }
    }

    fn read(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        let (_, document) = self.node.document()?;
        let contract = resolved_contract_from_document(&document)?;
        let values = bind_named(statement, params)?;
        let started = Instant::now();
        let guard = self.txn();
        let loaded;
        let records = match guard.as_ref() {
            Some(state) => &state.records,
            None => {
                loaded = self.node.load_rows_for(&statement.read_set)?;
                &loaded
            }
        };
        let rows = with_object_resolver(Some(Arc::clone(&self.node.objects)), || {
            database::execution::local::execute_prepared(statement, &values, Some(&contract), records)
        })?;
        self.node.perf.record("yyds.node.read", started.elapsed());
        Ok(rows)
    }

    fn write(&self, source: &str, tables: &[String], params: &BTreeMap<String, Value>) -> Result<usize> {
        let (version, document) = self.node.document()?;
        let parsed = parse_vos_write(source, Some(&document))?;
        let bound = parsed.bind(params)?;
        let contract = resolved_contract_from_document(&document)?;
        let declared: Vec<TableUniqueConstraint> = Vec::new();
        let merged = constraints::merge(constraints::from_contract(&contract)?, &declared);
        let ops = lower_write_program(&bound.program, &contract, Some(&document), &bound.values, &mut |value| Ok(value))?;
        let basis = (fingerprint(&document), u64::from(version));
        let started = Instant::now();

        let mut guard = self.txn();
        if let Some(state) = guard.as_mut() {
            let index = constraints::WriteIndex { index: &self.node.unique, basis, complete: true };
            let outcome = with_object_resolver(Some(Arc::clone(&self.node.objects)), || {
                execute_ops(&ops, &contract, &merged, &mut state.records, &bound.values, &Placement::Inline, Some(&index))
            })?;
            self.node.perf.record("yyds.node.write", started.elapsed());
            return Ok(outcome.affected);
        }
        drop(guard);

        self.node.gate.acquire();
        let _lease = GateGuard(&self.node.gate);
        let base = self.node.load_rows_for(tables)?;
        let mut records = base.clone();
        let index = constraints::WriteIndex { index: &self.node.unique, basis, complete: true };
        let outcome = with_object_resolver(Some(Arc::clone(&self.node.objects)), || {
            execute_ops(&ops, &contract, &merged, &mut records, &bound.values, &Placement::Inline, Some(&index))
        })?;
        self.node.perf.record("yyds.node.write", started.elapsed());
        self.node.commit_diff(&base, &records)?;
        Ok(outcome.affected)
    }
}

impl Drop for VosBackend {
    fn drop(&mut self) {
        self.release_lease();
    }
}

impl ServeBackend for VosBackend {
    fn info(&self) -> Result<Vec<u8>> {
        let schema_line = match self.node.schema() {
            Some(schema) => format!("schema.version={}\n", schema.version),
            None => "schema.version=\n".to_owned(),
        };
        let text = format!("product=yyds\nversion={}\nnamespace={VOS_NAMESPACE}\n{schema_line}", env!("CARGO_PKG_VERSION"));
        Ok(text.into_bytes())
    }

    fn schema(&self) -> Result<Option<SchemaVersion>> {
        Ok(self.node.schema())
    }

    fn ensure_schema(&self, version: u32, document: &str) -> Result<()> {
        self.node.ensure_schema(version, document)
    }

    /// 集群 schema 迁移需要成员协议先就绪。
    ///
    /// 目录在集群里是复制状态，立即重写、延后收敛与旧 revision 读取都必须由
    /// 成员共识和 peer transport 驱动；在此之前返回结构化拒绝，而不是只改
    /// 本节点目录造成分片间 schema 漂移。
    fn migrate_schema(&self, _version: u32, _document: &str, _renames_json: &str, _mode: u8) -> Result<()> {
        Err(Error::Unsupported(
            "YYDS cluster schema migration requires the CLUSTER member protocol; use a yydb node for DDL until it lands",
        ))
    }

    /// 延后迁移的旧行收敛同样依赖成员协议。
    fn compact_schemas(&self, _budget: u32) -> Result<u32> {
        Err(Error::Unsupported(
            "YYDS cluster schema compaction requires the CLUSTER member protocol; use a yydb node for DDL until it lands",
        ))
    }

    fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.ensure_open()?;
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let command = KeyValueCommand { namespace, key: key.as_bytes().to_vec(), action: KeyValueAction::Get };
        match self.node.executor.execute(None, command).map_err(Error::from)? {
            database::query::cluster_kv::KeyValueResult::Get(value) => Ok(value),
            _ => Err(Error::Protocol { message: "executor returned unexpected result for Get".to_owned() }),
        }
    }

    fn kv_put(&self, key: String, value: Vec<u8>) -> Result<()> {
        self.ensure_open()?;
        let namespace = Namespace(VOS_NAMESPACE.to_owned());
        let command = KeyValueCommand { namespace, key: key.into_bytes(), action: KeyValueAction::Put(value) };
        match self.node.executor.execute(None, command).map_err(Error::from)? {
            database::query::cluster_kv::KeyValueResult::Put { .. } => Ok(()),
            _ => Err(Error::Protocol { message: "executor returned unexpected result for Put".to_owned() }),
        }
    }

    fn prepare(&self, source: &str) -> Result<PreparedStatement> {
        self.ensure_open()?;
        prepare(source)
    }

    fn query_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        self.ensure_open()?;
        self.read(statement, params)
    }

    fn execute_prepared(&self, statement: &PreparedStatement, params: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        self.ensure_open()?;
        match statement.query_kind {
            QueryKind::Read => self.read(statement, params),
            QueryKind::Write => {
                let mut tables = statement.write_set.clone();
                tables.extend(statement.read_set.iter().cloned());
                self.write(&statement.source, &tables, params).map(|_| Vec::new())
            }
            QueryKind::Session => Err(Error::Protocol {
                message: "session boundary statements are served by the Txn* frames, not ExecutePrepared".to_owned(),
            }),
        }
    }

    fn begin(&self) -> Result<()> {
        self.ensure_open()?;
        let guard = self.txn();
        if guard.is_some() {
            return Err(Error::Protocol { message: "a transaction is already open on this connection".to_owned() });
        }
        // Fail before taking the gate so a missing schema never blocks other writers.
        self.node.document()?;
        drop(guard);
        self.enter_gate();
        let loaded = match self.node.load_rows() {
            Ok(rows) => rows,
            Err(error) => {
                self.exit_gate();
                return Err(error);
            }
        };
        *self.txn() = Some(TxnState { base: loaded.clone(), records: loaded, savepoints: Vec::new() });
        Ok(())
    }

    fn commit(&self) -> Result<()> {
        self.ensure_open()?;
        let Some(state) = self.txn().take()
        else {
            return Err(Error::Protocol { message: "no open transaction to commit".to_owned() });
        };
        let result = self.node.commit_diff(&state.base, &state.records);
        self.exit_gate();
        result
    }

    fn rollback(&self) -> Result<()> {
        self.ensure_open()?;
        if self.txn().take().is_none() {
            self.exit_gate();
            return Err(Error::Protocol { message: "no open transaction to roll back".to_owned() });
        }
        self.exit_gate();
        Ok(())
    }

    fn savepoint(&self, name: &str) -> Result<()> {
        self.ensure_open()?;
        let mut guard = self.txn();
        let Some(state) = guard.as_mut()
        else {
            return Err(Error::Protocol { message: "savepoints require an open transaction".to_owned() });
        };
        state.savepoints.push((name.to_owned(), state.records.clone()));
        Ok(())
    }

    fn rollback_to_savepoint(&self, name: &str) -> Result<()> {
        self.ensure_open()?;
        let mut guard = self.txn();
        let Some(state) = guard.as_mut()
        else {
            return Err(Error::Protocol { message: "savepoints require an open transaction".to_owned() });
        };
        let Some((_, saved)) = state.savepoints.iter().rev().find(|(label, _)| label == name)
        else {
            return Err(Error::Protocol { message: format!("unknown savepoint `{name}`") });
        };
        state.records = saved.clone();
        Ok(())
    }

    fn release_savepoint(&self, name: &str) -> Result<()> {
        self.ensure_open()?;
        let mut guard = self.txn();
        let Some(state) = guard.as_mut()
        else {
            return Err(Error::Protocol { message: "savepoints require an open transaction".to_owned() });
        };
        let Some(position) = state.savepoints.iter().rposition(|(label, _)| label == name)
        else {
            return Err(Error::Protocol { message: format!("unknown savepoint `{name}`") });
        };
        state.savepoints.remove(position);
        Ok(())
    }

    fn perf_report(&self) -> Result<String> {
        self.ensure_open()?;
        let mut report = report_json(&self.node.perf).map_err(|error| Error::Protocol { message: error.to_string() })?;
        let schema_version = self.node.schema().map(|schema| schema.version);
        let open = self.txn().is_some();
        if let Some(object) = report.as_object_mut() {
            object.insert("node".to_owned(), serde_json::json!({ "namespace": VOS_NAMESPACE, "schemaVersion": schema_version, "transactionOpen": open }));
        }
        serde_json::to_string(&report).map_err(|error| Error::Protocol { message: error.to_string() })
    }

    fn close(&self) -> Result<()> {
        self.release_lease();
        Ok(())
    }
}

/// 为每条连接打开 [`VosBackend`]，共享同一节点状态。
pub struct VosBackendFactory {
    node: Arc<VosNode>,
}

impl VosBackendFactory {
    /// 用分片执行器与可选 catalog schema 服务建立工厂。
    pub fn new(executor: Arc<dyn KeyValueExecutor>, catalog: Option<CatalogSchemaService>) -> Self {
        Self { node: Arc::new(VosNode::new(executor, catalog)) }
    }

    /// 共享节点状态（宿主用于 schema 安装、诊断与关停）。
    pub fn node(&self) -> &Arc<VosNode> {
        &self.node
    }
}

impl BackendFactory for VosBackendFactory {
    fn open(&self, _peer_is_loopback: bool) -> Box<dyn ServeBackend> {
        Box::new(VosBackend::new(Arc::clone(&self.node)))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::mpsc::{RecvTimeoutError, channel},
        time::{Duration, SystemTime},
    };

    use database::{
        client::yy::Client,
        server::yydb::{AuthBootstrap, ServeOptions, WireServe},
    };

    use super::*;

    const SCHEMA: &str = "\
table Item {
    @@id: uuid,
    name: utf8,
    qty: i64,
}
";

    const INSERT: &str = "Item.insert({ id: $id, name: $name, qty: $qty })";
    const BY_ID: &str = "Item.all().filter(i => i.id == $id).collect()";

    struct TempDir(PathBuf);

    impl TempDir {
        fn create(label: &str) -> Self {
            let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock").as_nanos();
            let path = std::env::temp_dir().join(format!("yyds-vos-{label}-{nonce}"));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A running single-shard node with a published schema.
    struct Node {
        serve: Option<WireServe>,
        endpoint: String,
        _dir: TempDir,
    }

    impl Node {
        fn start(label: &str) -> Self {
            let dir = TempDir::create(label);
            let executor = crate::runtime::open_executor(&dir.0, None).expect("open local shard");
            let catalog = CatalogSchemaService::memory();
            let factory = Arc::new(VosBackendFactory::new(executor, Some(catalog)));
            factory.node().ensure_schema(1, SCHEMA).expect("install schema");
            let options = ServeOptions { bind: "127.0.0.1:0".parse().expect("addr"), bootstrap: AuthBootstrap { tokens: Vec::new() }, anonymous_loopback: true, label: "yyds-vos-test" };
            let serve = WireServe::start(options, factory).expect("start wire server");
            let endpoint = serve.address().to_string();
            Self { serve: Some(serve), endpoint, _dir: dir }
        }

        fn client(&self) -> Client {
            Client::connect(self.endpoint.clone()).expect("connect")
        }
    }

    impl Drop for Node {
        fn drop(&mut self) {
            if let Some(serve) = self.serve.take() {
                let _ = serve.stop();
            }
        }
    }

    fn item_params(id: uuid::Uuid, name: &str, qty: i64) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("id".to_owned(), Value::Uuid(id)),
            ("name".to_owned(), Value::Text(name.to_owned())),
            ("qty".to_owned(), Value::I64(qty)),
        ])
    }

    fn query_items(client: &Client) -> Vec<BTreeMap<String, Value>> {
        client.prepare("Item.all().collect()").and_then(|sid| client.execute_prepared(&sid, &BTreeMap::new())).expect("read items")
    }

    #[test]
    fn insert_reads_back_with_typed_values() {
        let node = Node::start("crud");
        let client = node.client();
        let id = uuid::Uuid::now_v7();
        let statement = client.prepare(INSERT).expect("prepare insert");
        client.execute_prepared(&statement, &item_params(id, "widget", 7)).expect("insert");

        let read = client.prepare(BY_ID).expect("prepare read");
        let rows = client.execute_prepared(&read, &BTreeMap::from([("id".to_owned(), Value::Uuid(id))])).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], Value::Uuid(id), "the uuid survives the tagged JSON wire codec");
        assert_eq!(rows[0]["name"], Value::Text("widget".to_owned()));
        assert_eq!(rows[0]["qty"], Value::I64(7));

        // The row really lives in the shard under the shared record key.
        let shard = node._dir.0.join(format!("{}.yykv", crate::runtime::LOCAL_SHARD_ID));
        assert!(shard.exists(), "records land in the node shard file");
    }

    #[test]
    fn transactions_roll_back_and_commit() {
        let node = Node::start("txn");
        let client = node.client();
        let statement = client.prepare(INSERT).expect("prepare");

        client.begin().expect("begin");
        client.execute_prepared(&statement, &item_params(uuid::Uuid::now_v7(), "rolled-back", 1)).expect("write in tx");
        client.rollback().expect("rollback");
        assert!(query_items(&client).is_empty(), "a rolled back insert must leave no row");

        client.begin().expect("begin");
        let committed = uuid::Uuid::now_v7();
        client.execute_prepared(&statement, &item_params(committed, "committed", 2)).expect("write in tx");
        client.commit().expect("commit");
        let rows = query_items(&client);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], Value::Uuid(committed));
    }

    #[test]
    fn savepoints_restore_only_the_marked_prefix() {
        let node = Node::start("savepoint");
        let client = node.client();
        let statement = client.prepare(INSERT).expect("prepare");
        let first = uuid::Uuid::now_v7();

        client.begin().expect("begin");
        client.execute_prepared(&statement, &item_params(first, "kept", 1)).expect("insert before savepoint");
        client.savepoint("sp1").expect("savepoint");
        client.execute_prepared(&statement, &item_params(uuid::Uuid::now_v7(), "dropped", 2)).expect("insert after savepoint");
        client.rollback_to_savepoint("sp1").expect("rollback to savepoint");
        let rows = query_items(&client);
        assert_eq!(rows.len(), 1, "only the row written before the savepoint survives the rollback");
        assert_eq!(rows[0]["id"], Value::Uuid(first));
        client.release_savepoint("sp1").expect("release");
        client.commit().expect("commit");
        assert_eq!(query_items(&client).len(), 1);
    }

    #[test]
    fn transaction_errors_are_reported_without_leaving_a_transaction_open() {
        let node = Node::start("txn-errors");
        let client = node.client();
        assert!(client.commit().is_err(), "commit without begin must fail");
        assert!(client.rollback_to_savepoint("missing").is_err());
        client.begin().expect("begin");
        assert!(client.begin().is_err(), "nested begin must fail");
        assert!(client.rollback_to_savepoint("missing").is_err(), "unknown savepoint must fail");
        client.rollback().expect("rollback");
        assert!(client.rollback().is_err(), "double rollback must fail");
    }

    #[test]
    fn explicit_close_releases_the_write_lease_to_the_next_session() {
        let node = Node::start("lease");
        let holder = node.client();
        let successor = node.client();

        holder.begin().expect("the first session takes the write lease");
        let (sender, receiver) = channel();
        let handle = std::thread::spawn(move || {
            let outcome = successor.begin();
            let _ = successor.rollback();
            sender.send(outcome.map_err(|error| error.to_string())).expect("report successor outcome");
        });
        assert!(
            matches!(receiver.recv_timeout(Duration::from_millis(200)), Err(RecvTimeoutError::Timeout)),
            "a successor must not acquire the lease while the holder keeps it"
        );

        holder.close().expect("explicit close releases the lease");
        let outcome = receiver.recv_timeout(Duration::from_secs(5)).expect("successor is admitted after close");
        assert!(outcome.is_ok(), "successor begin failed: {outcome:?}");
        handle.join().expect("successor thread");

        assert!(matches!(holder.begin(), Err(Error::SessionClosed)), "the released session is rejected");
    }

    #[test]
    fn perf_report_exposes_node_operators() {
        let node = Node::start("perf");
        let client = node.client();
        let statement = client.prepare(INSERT).expect("prepare");
        client.execute_prepared(&statement, &item_params(uuid::Uuid::now_v7(), "perf", 1)).expect("insert");
        query_items(&client);

        let report: serde_json::Value = serde_json::from_str(&client.perf_report().expect("perf report")).expect("json");
        assert_eq!(report["node"]["schemaVersion"], 1);
        let rows = report["rows"].as_array().expect("rows");
        assert!(rows.iter().any(|row| row["key"] == "yyds.node.read"), "reads are recorded: {report}");
        assert!(rows.iter().any(|row| row["key"] == "yyds.node.write"), "writes are recorded: {report}");
        assert!(rows.iter().any(|row| row["key"] == "yyds.node.commit"), "commits are recorded: {report}");
    }
}
