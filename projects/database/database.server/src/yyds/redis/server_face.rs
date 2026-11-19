//! 服务器面命令与监听器级客户端登记表。
//!
//! 这一层覆盖 `CLIENT` / `COMMAND` / `INFO` / `CONFIG` / `ACL`。口径按真机抓帧固定：
//! `CLIENT LIST` 是**单条** bulk、行间 `\n`，`CLIENT GETNAME` 无名称时回 null bulk，
//! `CLIENT KILL` 单参数形回 `OK`、带过滤器形回计数。监听器和嵌入式绑定共用这套实现，
//! 语言侧只保留转发。
//!
//! 连接登记表是监听器级的：`CLIENT LIST`、`CLIENT INFO` 和 `CLIENT KILL` 都要看到
//! 别的连接，所以每条连接的可变会话字段放在共享的 [`ClientRecord`] 里，而不是留在
//! 自己的读循环栈上。

use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use database_execution::cluster::{
    KeyValueExecutor,
    redis::{
        auth::AuthState,
        introspection::{self, WaitPlan},
        persistence::{PersistenceAction, PersistenceState},
        reply::{glob_match, write_bulk, write_error, write_null},
        session::{self, key_revision, watched_keys_modified},
        string as strings,
    },
};
use database_protocol_redis::codec::{CommandError, Reply};
use database_storage::cluster_kv::{now_unix_ms, redis::namespace as redis_namespace};
use database_types::cluster::Namespace;

use super::{
    connection::ConnectionState,
    pubsub::{ClientSink, PubSubState},
    resp::Request,
};

/// 一条连接的会话投影。
///
/// 只保存 `CLIENT LIST` 需要读取的字段。值全部来自连接自己的读循环，其它连接通过
/// 互斥量读取，因此字段更新必须整块进行，避免 `CLIENT LIST` 读到半新半旧的组合。
#[derive(Debug)]
pub(crate) struct ClientState {
    /// `CLIENT SETNAME` 设置的名字，空名按未设置处理（真机语义）。
    name: Option<Vec<u8>>,
    /// `CLIENT SETINFO LIB-NAME` 写入的客户端库名。
    lib_name: Vec<u8>,
    /// `CLIENT SETINFO LIB-VER` 写入的客户端库版本。
    lib_ver: Vec<u8>,
    /// 最近一条命令的 `parent|sub` 形式，供 `CLIENT LIST` 的 `cmd=` 字段使用。
    last_command: String,
    /// 已执行的命令条数，供 `tot-cmds=` 使用。
    command_count: u64,
    /// `SELECT` 选中的库号。
    database: u8,
    /// 连接协商出的 RESP 版本。
    protocol: u8,
    /// 最近一次收到命令的时刻，供 `idle=` 使用。
    last_activity: Instant,
    /// `MULTI` 队列长度；`None` 表示不在事务中，对应真机的 `multi=-1`。
    multi: Option<usize>,
    /// `CLIENT NO-EVICT` 的开关状态，目前只记录不回显。
    no_evict: bool,
    /// `CLIENT NO-TOUCH` 的开关状态，目前只记录不回显。
    no_touch: bool,
}

/// 一条连接在监听器登记表里的身份。
pub(crate) struct ClientRecord {
    /// 连接 id，`CLIENT ID` 与 `CLIENT LIST` 的 `id=` 使用同一个值。
    pub(crate) id: u64,
    /// 发布/订阅表分配的内部 id，用来查询订阅计数。
    pub(crate) pubsub_id: u64,
    /// 对端地址，`addr=` 与单参数 `CLIENT KILL` 使用它。
    remote: String,
    /// 本端地址，`laddr=` 与 `CLIENT LIST LADDR` 过滤使用它。
    local: String,
    /// 连接建立时刻，供 `age=` 使用。
    created: Instant,
    /// 写侧句柄，`CLIENT KILL` 通过它关闭 socket；嵌入式会话没有 socket。
    sink: Option<Arc<ClientSink>>,
    /// 是否已被 `CLIENT KILL` 选中。监听器靠关闭 socket 结束连接，无 socket 的
    /// 嵌入式会话由宿主读取这个标志后自行断开。
    closed: AtomicBool,
    /// 可变会话字段。
    state: Mutex<ClientState>,
}

/// 只打印身份字段：写侧句柄不实现 `Debug`，也没有诊断价值。
impl std::fmt::Debug for ClientRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ClientRecord").field("id", &self.id).field("remote", &self.remote).finish_non_exhaustive()
    }
}

impl ClientRecord {
    fn new(id: u64, pubsub_id: u64, remote: String, local: String, sink: Option<Arc<ClientSink>>) -> Self {
        let now = Instant::now();
        Self {
            id,
            pubsub_id,
            remote,
            local,
            created: now,
            sink,
            closed: AtomicBool::new(false),
            state: Mutex::new(ClientState {
                name: None,
                lib_name: Vec::new(),
                lib_ver: Vec::new(),
                last_command: "client|list".into(),
                command_count: 0,
                database: 0,
                protocol: 2,
                last_activity: now,
                multi: None,
                no_evict: false,
                no_touch: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ClientState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// 记下一条命令的上下文并推进活动时间。
    pub(crate) fn touch(&self, context: String) {
        let mut state = self.lock();
        state.last_command = context;
        state.last_activity = Instant::now();
        state.command_count += 1;
    }

    /// 更新 `CLIENT SETNAME` 的名字。
    pub(crate) fn set_name(&self, name: Option<Vec<u8>>) {
        self.lock().name = name;
    }

    /// 记录 `CLIENT SETINFO` 的库名或版本。
    fn set_lib(&self, name: bool, value: Vec<u8>) {
        let mut state = self.lock();
        if name {
            state.lib_name = value;
        }
        else {
            state.lib_ver = value;
        }
    }

    /// 读取当前名字，`CLIENT GETNAME` 使用。
    fn name(&self) -> Option<Vec<u8>> {
        self.lock().name.clone()
    }

    /// `SELECT` 后同步库号。
    pub(crate) fn set_database(&self, database: u8) {
        self.lock().database = database;
    }

    /// `HELLO` / `RESET` 后同步协议版本。
    pub(crate) fn set_protocol(&self, protocol: u8) {
        self.lock().protocol = protocol;
    }

    /// `MULTI` / `EXEC` / `DISCARD` 后同步队列状态。
    pub(crate) fn set_multi(&self, queue: Option<usize>) {
        self.lock().multi = queue;
    }

    fn set_flag(&self, evict: bool, on: bool) {
        let mut state = self.lock();
        if evict {
            state.no_evict = on;
        }
        else {
            state.no_touch = on;
        }
    }

    /// 标记这条连接已被关闭，并立即关闭它的 socket。
    ///
    /// 无 socket 的嵌入式会话只留下标志，宿主读完 `CLIENT LIST` 语义所需的状态后
    /// 自行断开。
    fn kill(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(sink) = &self.sink {
            sink.shutdown();
        }
    }

    /// 是否已被 `CLIENT KILL` 选中。
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// 无写侧的会话由嵌入式宿主登记，`CLIENT KILL` 后需要宿主回收。
    fn detached(&self) -> bool {
        self.sink.is_none()
    }

    fn age_seconds(&self) -> u64 {
        self.created.elapsed().as_secs()
    }

    /// 一行 `CLIENT LIST` 文本，字段顺序与真机输出一致。
    fn line(&self, sub: usize, psub: usize) -> String {
        let state = self.lock();
        let name = state.name.as_ref().map(|value| String::from_utf8_lossy(value).into_owned()).unwrap_or_default();
        let lib_name = String::from_utf8_lossy(&state.lib_name).into_owned();
        let lib_ver = String::from_utf8_lossy(&state.lib_ver).into_owned();
        // 真机把处于订阅态的连接标成 `flags=P`，其余为 `flags=N`。
        let flags = if sub + psub > 0 { "P" } else { "N" };
        format!(
            "id={} addr={} laddr={} fd={} name={} age={} idle={} flags={flags} db={} sub={} psub={} ssub=0 multi={} watch=0 \
             qbuf=0 qbuf-free=0 argv-mem=0 multi-mem=0 rbs=1024 rbp=0 obl=0 oll=0 omem=0 omem-shared=0 omem-unshared=0 \
             tot-mem=0 events=r cmd={} user=default redir=-1 resp={} lib-name={} lib-ver={} io-thread=0 tot-net-in=0 tot-net-out=0 \
             tot-cmds={} read-events=0 avg-pipeline-len-sum=0 avg-pipeline-len-cnt=0",
            self.id,
            self.remote,
            self.local,
            self.id,
            name,
            self.age_seconds(),
            state.last_activity.elapsed().as_secs(),
            state.database,
            sub,
            psub,
            state.multi.map_or(-1, |queued| queued as i64),
            state.last_command,
            state.protocol,
            lib_name,
            lib_ver,
            state.command_count
        )
    }
}

/// 监听器级客户端登记表。
///
/// 除连接投影外，这里还持有 `CONFIG` 的可变值和 `INFO` 的累计计数：两者都是服务器级
/// 状态，必须和连接一样由同一监听器的所有连接共享。
#[derive(Debug)]
pub struct ClientRegistry {
    next_id: AtomicU64,
    connections: AtomicU64,
    commands: AtomicU64,
    net_input: AtomicU64,
    reads: AtomicU64,
    entries: Mutex<Vec<Arc<ClientRecord>>>,
    config: Mutex<Vec<(&'static str, String)>>,
    /// 服务器级 `requirepass`，`CONFIG SET requirepass` 由宿主写回，认证判定读取它。
    requirepass: Mutex<Option<String>>,
    /// 服务器级持久化状态：最后保存时刻与后台保存进行中标记。
    persistence: Mutex<PersistenceState>,
    started: Instant,
    run_id: String,
    port: u16,
    bind: String,
}

impl ClientRegistry {
    /// 创建一个登记表；`port` / `bind` 只用于 `INFO` 的监听行。
    pub fn new(port: u16, bind: impl Into<String>) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            connections: AtomicU64::new(0),
            commands: AtomicU64::new(0),
            net_input: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            entries: Mutex::new(Vec::new()),
            config: Mutex::new(CONFIG_DEFAULTS.iter().map(|(name, value)| (*name, (*value).to_string())).collect()),
            requirepass: Mutex::new(None),
            persistence: Mutex::new(PersistenceState::new(
                SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0),
            )),
            started: Instant::now(),
            run_id: generate_run_id(),
            port,
            bind: bind.into(),
        }
    }

    /// 没有监听器时的独立登记表，端口与地址留空。
    pub fn standalone() -> Self {
        Self::new(0, "")
    }

    /// 写回服务器级 `requirepass`；`None` 表示未配置口令。
    pub fn set_requirepass(&self, requirepass: Option<String>) {
        *self.requirepass.lock().unwrap_or_else(|poison| poison.into_inner()) = requirepass;
    }

    /// 当前服务器级 `requirepass`。
    pub fn requirepass(&self) -> Option<String> {
        self.requirepass.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
    }

    /// 写入监听器级配置值，供嵌入式宿主在启动时对齐自己的运行时设置。
    ///
    /// 只更新已注册的选项：`CONFIG GET` 的选项表归这里所有，宿主不得自建第二张表。
    pub fn set_config(&self, name: &str, value: &str) {
        let mut config = self.config.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = config.iter_mut().find(|(known, _)| *known == name) {
            entry.1 = value.to_owned();
        }
    }

    /// 同步保存完成：记录保存时刻。
    pub fn note_saved(&self, now_unix: i64) {
        self.persistence.lock().unwrap_or_else(|poison| poison.into_inner()).mark_saved(now_unix);
    }

    /// 后台保存完成：清进行中标记并记录保存时刻。
    pub fn note_background_finished(&self, now_unix: i64) {
        self.persistence.lock().unwrap_or_else(|poison| poison.into_inner()).finish_background(now_unix);
    }

    /// 宿主在服务器真正开始监听时校正 `LASTSAVE` 的时间基准。
    pub fn note_server_started(&self, started_unix: i64) {
        self.persistence.lock().unwrap_or_else(|poison| poison.into_inner()).set_started(started_unix);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<ClientRecord>>> {
        self.entries.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// 登记一条新连接并分配 id。
    pub(crate) fn attach(&self, sink: Arc<ClientSink>, pubsub_id: u64, remote: String, local: String) -> Arc<ClientRecord> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        self.connections.fetch_add(1, Ordering::AcqRel);
        let record = Arc::new(ClientRecord::new(id, pubsub_id, remote, local, Some(sink)));
        self.lock().push(Arc::clone(&record));
        record
    }

    /// 用宿主给定的 id 登记一条没有 socket 的会话。
    ///
    /// 嵌入式宿主按自己的连接号登记，`CLIENT ID` / `CLIENT LIST` / `CLIENT KILL`
    /// 因此能直接命中宿主侧的连接，不需要额外的 id 映射。
    pub fn attach_detached_with(&self, id: u64) -> Arc<ClientRecord> {
        self.next_id.fetch_max(id.saturating_add(1), Ordering::AcqRel);
        self.connections.fetch_add(1, Ordering::AcqRel);
        let address = "127.0.0.1:0".to_string();
        let record = Arc::new(ClientRecord::new(id, id, address.clone(), address, None));
        self.lock().push(Arc::clone(&record));
        record
    }

    /// 连接结束时移除登记，避免 `CLIENT LIST` 读到已关闭的连接。
    pub fn detach(&self, id: u64) {
        self.lock().retain(|entry| entry.id != id);
    }

    /// 回收被 `CLIENT KILL` 关闭的无 socket 会话并返回它们的 id。
    ///
    /// 监听器连接会在读循环结束时自行 [`detach`](Self::detach)，这里只处理嵌入式
    /// 会话，宿主据返回的 id 关闭对应的宿主连接。
    pub fn take_killed(&self) -> Vec<u64> {
        let mut entries = self.lock();
        let mut killed = Vec::new();
        entries.retain(|entry| {
            if entry.is_closed() && entry.detached() {
                killed.push(entry.id);
                false
            }
            else {
                true
            }
        });
        killed
    }

    fn snapshot(&self) -> Vec<Arc<ClientRecord>> {
        self.lock().clone()
    }

    fn count(&self) -> usize {
        self.lock().len()
    }

    /// 统计一条命令，`INFO stats` 的 `total_commands_processed` 使用。
    pub(crate) fn bump_command(&self) {
        self.commands.fetch_add(1, Ordering::AcqRel);
    }

    /// 统计一次读入，`INFO stats` 的字节数与读次数使用。
    pub(crate) fn add_input(&self, bytes: usize) {
        self.net_input.fetch_add(bytes as u64, Ordering::AcqRel);
        self.reads.fetch_add(1, Ordering::AcqRel);
    }
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self::standalone()
    }
}

/// 40 位十六进制的进程标识，形状与真机的 `run_id` 一致。
fn generate_run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|delta| delta.as_nanos()).unwrap_or(0);
    format!("{nanos:040x}")
}

/// `CLIENT` 的完整子命令面。
///
/// 真机把客户端名字限制在可打印 ASCII，空名等价于清除；`SETINFO` 只接受
/// `LIB-NAME` / `LIB-VER` 并把值写进 `CLIENT LIST`。返回 `false` 表示这条连接已被
/// `CLIENT KILL` 选中，写完回复后必须关闭。
pub(crate) fn handle_client(
    stream: &mut dyn Write,
    request: &Request<'_>,
    state: &ConnectionState,
    registry: &ClientRegistry,
    pubsub: &PubSubState,
) -> io::Result<bool> {
    let count = request.arguments.len();
    let Some(raw) = request.arguments.get(1)
    else {
        return write_arity(stream, "client").map(|_| true);
    };
    let subcommand = String::from_utf8_lossy(raw).to_ascii_lowercase();
    match subcommand.as_str() {
        "setinfo" => {
            if count != 4 {
                return write_arity(stream, "client|setinfo").map(|_| true);
            }
            // 真机只认识 LIB-NAME / LIB-VER，其它属性报 Unrecognized option。
            let attribute = request.arguments[2];
            let is_name = attribute.eq_ignore_ascii_case(b"LIB-NAME");
            if !is_name && !attribute.eq_ignore_ascii_case(b"LIB-VER") {
                write_error(stream, &format!("Unrecognized option '{}'", String::from_utf8_lossy(attribute)))?;
                return Ok(true);
            }
            state.record.set_lib(is_name, request.arguments[3].to_vec());
            stream.write_all(b"+OK\r\n")?;
            Ok(true)
        }
        "setname" => {
            if count != 3 {
                return write_arity(stream, "client|setname").map(|_| true);
            }
            let name = request.arguments[2];
            if !valid_client_name(name) {
                write_error(stream, "Client names cannot contain spaces, newlines or special characters.")?;
                return Ok(true);
            }
            state.record.set_name(if name.is_empty() { None } else { Some(name.to_vec()) });
            stream.write_all(b"+OK\r\n")?;
            Ok(true)
        }
        "getname" => {
            if count != 2 {
                return write_arity(stream, "client|getname").map(|_| true);
            }
            match state.record.name() {
                Some(name) => write_bulk(stream, &name)?,
                None => write_null(stream, state.protocol)?,
            }
            Ok(true)
        }
        "id" => {
            if count != 2 {
                return write_arity(stream, "client|id").map(|_| true);
            }
            stream.write_all(format!(":{}\r\n", state.record.id).as_bytes())?;
            Ok(true)
        }
        "list" => handle_client_list(stream, request, registry, pubsub).map(|_| true),
        "info" => {
            if count != 2 {
                return write_arity(stream, "client|info").map(|_| true);
            }
            let (sub, psub) = pubsub.counts(state.record.pubsub_id);
            write_bulk(stream, state.record.line(sub, psub).as_bytes())?;
            Ok(true)
        }
        "no-evict" | "no-touch" => {
            if count != 3 {
                return write_arity(stream, &format!("client|{subcommand}")).map(|_| true);
            }
            let value = request.arguments[2];
            let on = if value.eq_ignore_ascii_case(b"ON") {
                true
            }
            else if value.eq_ignore_ascii_case(b"OFF") {
                false
            }
            else {
                write_error(stream, "syntax error")?;
                return Ok(true);
            };
            state.record.set_flag(subcommand == "no-evict", on);
            stream.write_all(b"+OK\r\n")?;
            Ok(true)
        }
        "unpause" => {
            if count != 2 {
                return write_arity(stream, "client|unpause").map(|_| true);
            }
            stream.write_all(b"+OK\r\n")?;
            Ok(true)
        }
        "kill" => handle_client_kill(stream, request, state, registry, pubsub),
        _ => {
            write_error(stream, &format!("unknown subcommand '{}'. Try CLIENT HELP.", String::from_utf8_lossy(raw)))?;
            Ok(true)
        }
    }
}

/// 约定：失败时返回 `true`，表示连接保持打开。
fn write_arity(stream: &mut dyn Write, name: &str) -> io::Result<()> {
    stream.write_all(format!("-ERR wrong number of arguments for '{name}' command\r\n").as_bytes())
}

/// 真机只接受 0x21..=0x7e 的客户端名，空名合法且等价于清除。
fn valid_client_name(name: &[u8]) -> bool {
    name.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

/// `CLIENT LIST [TYPE type] [ID id] [LADDR addr] [USER user]`。
fn handle_client_list(stream: &mut dyn Write, request: &Request<'_>, registry: &ClientRegistry, pubsub: &PubSubState) -> io::Result<()> {
    let mut filter_type: Option<String> = None;
    let mut filter_id: Option<u64> = None;
    let mut filter_laddr: Option<String> = None;
    let raw = &request.arguments[2..];
    let mut index = 0;
    while index < raw.len() {
        let Some(value) = raw.get(index + 1)
        else {
            write_error(stream, "syntax error")?;
            return Ok(());
        };
        let option = String::from_utf8_lossy(raw[index]).to_ascii_lowercase();
        match option.as_str() {
            "type" => {
                let kind = String::from_utf8_lossy(value).to_ascii_lowercase();
                if !matches!(kind.as_str(), "normal" | "master" | "replica" | "pubsub") {
                    write_error(stream, "syntax error")?;
                    return Ok(());
                }
                filter_type = Some(kind);
            }
            "id" => match parse_positive(value) {
                Some(id) => filter_id = Some(id),
                None => {
                    write_error(stream, "syntax error")?;
                    return Ok(());
                }
            },
            "laddr" => filter_laddr = Some(String::from_utf8_lossy(value).into_owned()),
            "user" => {
                if !String::from_utf8_lossy(value).eq_ignore_ascii_case("default") {
                    write_error(stream, "syntax error")?;
                    return Ok(());
                }
            }
            _ => {
                write_error(stream, "syntax error")?;
                return Ok(());
            }
        }
        index += 2;
    }
    let mut lines = Vec::new();
    for record in registry.snapshot() {
        let (sub, psub) = pubsub.counts(record.pubsub_id);
        if filter_id.is_some_and(|id| record.id != id) {
            continue;
        }
        if filter_laddr.as_deref().is_some_and(|addr| record.local != addr) {
            continue;
        }
        if let Some(kind) = &filter_type {
            let actual = if sub + psub > 0 { "pubsub" } else { "normal" };
            if actual != kind.as_str() {
                continue;
            }
        }
        lines.push(record.line(sub, psub));
    }
    // 真机回单条 bulk，行间用 `\n` 连接。
    write_bulk(stream, lines.join("\n").as_bytes())
}

/// `CLIENT KILL`：单参数形按 `addr` 命中并回 `OK`，过滤器形按条件命中并回计数。
fn handle_client_kill(
    stream: &mut dyn Write,
    request: &Request<'_>,
    state: &ConnectionState,
    registry: &ClientRegistry,
    pubsub: &PubSubState,
) -> io::Result<bool> {
    let count = request.arguments.len();
    if count == 2 {
        write_arity(stream, "client|kill")?;
        return Ok(true);
    }
    let entries = registry.snapshot();
    let self_id = state.record.id;
    if count == 3 {
        // 单参数形按完整 `host:port` 匹配，找不到时真机报 No such client。
        let address = String::from_utf8_lossy(request.arguments[2]).into_owned();
        let Some(target) = entries.iter().find(|entry| entry.remote == address)
        else {
            write_error(stream, "No such client")?;
            return Ok(true);
        };
        if target.id != self_id {
            target.kill();
        }
        stream.write_all(b"+OK\r\n")?;
        return Ok(target.id == self_id);
    }
    let mut filters: Vec<(String, String)> = Vec::new();
    let mut skipme = true;
    let raw = &request.arguments[2..];
    let mut index = 0;
    while index < raw.len() {
        let Some(value) = raw.get(index + 1)
        else {
            write_error(stream, "syntax error")?;
            return Ok(true);
        };
        let option = String::from_utf8_lossy(raw[index]).to_ascii_lowercase();
        let value = String::from_utf8_lossy(value).into_owned();
        match option.as_str() {
            "id" => {
                if parse_positive(raw[index + 1]).is_none() {
                    write_error(stream, "client-id should be greater than 0")?;
                    return Ok(true);
                }
            }
            "type" => {
                if !matches!(value.to_ascii_lowercase().as_str(), "normal" | "master" | "replica" | "pubsub") {
                    write_error(stream, &format!("Unknown client type '{value}'"))?;
                    return Ok(true);
                }
            }
            "user" => {
                if !value.eq_ignore_ascii_case("default") {
                    write_error(stream, &format!("No such user '{value}'"))?;
                    return Ok(true);
                }
            }
            "addr" | "laddr" => {}
            "maxage" => {
                if parse_integer_text(&value).is_none() {
                    write_error(stream, "maxage is not an integer or out of range")?;
                    return Ok(true);
                }
            }
            "skipme" => {
                let lowered = value.to_ascii_lowercase();
                if lowered != "yes" && lowered != "no" {
                    write_error(stream, "syntax error")?;
                    return Ok(true);
                }
                skipme = lowered == "yes";
            }
            _ => {
                write_error(stream, "syntax error")?;
                return Ok(true);
            }
        }
        filters.push((option, value));
        index += 2;
    }
    let mut matched = 0_i64;
    let mut self_matched = false;
    for record in &entries {
        if record.id == self_id && skipme {
            continue;
        }
        let (sub, psub) = pubsub.counts(record.pubsub_id);
        let mut hit = true;
        for (option, value) in &filters {
            let keep = match option.as_str() {
                "id" => parse_positive(value.as_bytes()) == Some(record.id),
                "addr" => record.remote == *value,
                "laddr" => record.local == *value,
                "user" => true,
                "maxage" => record.age_seconds() as i64 >= parse_integer_text(value).unwrap_or(0),
                "type" => {
                    let actual = if sub + psub > 0 { "pubsub" } else { "normal" };
                    actual == value.to_ascii_lowercase()
                }
                _ => true,
            };
            if !keep {
                hit = false;
                break;
            }
        }
        if !hit {
            continue;
        }
        matched += 1;
        if record.id == self_id {
            self_matched = true;
        }
        else {
            record.kill();
        }
    }
    stream.write_all(format!(":{matched}\r\n").as_bytes())?;
    Ok(!self_matched)
}

fn parse_positive(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value = text.parse::<u64>().ok()?;
    (value > 0 && value.to_string() == text).then_some(value)
}

fn parse_integer_text(text: &str) -> Option<i64> {
    let value = text.parse::<i64>().ok()?;
    (value.to_string() == text).then_some(value)
}

/// `CLIENT LIST` 的 `cmd=` 字段：容器命令显示 `parent|sub`，其余只显示命令名。
pub(crate) fn command_context(command: &[u8], arguments: &[&[u8]]) -> String {
    let name = String::from_utf8_lossy(command).to_ascii_lowercase();
    const CONTAINERS: &[&str] =
        &["acl", "client", "command", "config", "function", "latency", "memory", "object", "pubsub", "script", "xgroup", "xinfo"];
    if CONTAINERS.contains(&name.as_str()) {
        if let Some(subcommand) = arguments.get(1) {
            return format!("{name}|{}", String::from_utf8_lossy(subcommand).to_ascii_lowercase());
        }
    }
    name
}

// ---------------------------------------------------------------------------
// `COMMAND`
// ---------------------------------------------------------------------------

/// 嵌套元数据里的一个值。
enum SpecValue {
    Text(&'static str),
    Int(i64),
    /// 只出现在 `flags` 位置：编码成 simple string 数组。
    Texts(&'static [&'static str]),
    /// 编码成 key/value 对数组，嵌套时继续按对展开。
    Map(&'static [(&'static str, SpecValue)]),
}

/// 一条命令的十元组元数据。
struct CommandSpec {
    name: &'static str,
    arity: i64,
    flags: &'static [&'static str],
    first: i64,
    last: i64,
    step: i64,
    acl: &'static [&'static str],
    tips: &'static [&'static str],
    key_specs: &'static [&'static [(&'static str, SpecValue)]],
    subcommands: &'static [CommandSpec],
}

fn write_spec_map(stream: &mut dyn Write, pairs: &[(&'static str, SpecValue)]) -> io::Result<()> {
    stream.write_all(format!("*{}\r\n", pairs.len() * 2).as_bytes())?;
    for (key, value) in pairs {
        write_bulk(stream, key.as_bytes())?;
        write_spec_value(stream, value)?;
    }
    Ok(())
}

fn write_spec_value(stream: &mut dyn Write, value: &SpecValue) -> io::Result<()> {
    match value {
        SpecValue::Text(text) => write_bulk(stream, text.as_bytes()),
        SpecValue::Int(number) => stream.write_all(format!(":{number}\r\n").as_bytes()),
        SpecValue::Texts(values) => {
            stream.write_all(format!("*{}\r\n", values.len()).as_bytes())?;
            for value in values.iter() {
                stream.write_all(format!("+{value}\r\n").as_bytes())?;
            }
            Ok(())
        }
        SpecValue::Map(pairs) => write_spec_map(stream, pairs),
    }
}

fn write_command_spec(stream: &mut dyn Write, spec: &CommandSpec) -> io::Result<()> {
    stream.write_all(b"*10\r\n")?;
    write_bulk(stream, spec.name.as_bytes())?;
    stream.write_all(format!(":{}\r\n", spec.arity).as_bytes())?;
    stream.write_all(format!("*{}\r\n", spec.flags.len()).as_bytes())?;
    for flag in spec.flags {
        stream.write_all(format!("+{flag}\r\n").as_bytes())?;
    }
    stream.write_all(format!(":{}\r\n:{}\r\n:{}\r\n", spec.first, spec.last, spec.step).as_bytes())?;
    stream.write_all(format!("*{}\r\n", spec.acl.len()).as_bytes())?;
    for category in spec.acl {
        stream.write_all(format!("+{category}\r\n").as_bytes())?;
    }
    stream.write_all(format!("*{}\r\n", spec.tips.len()).as_bytes())?;
    for tip in spec.tips {
        write_bulk(stream, tip.as_bytes())?;
    }
    stream.write_all(format!("*{}\r\n", spec.key_specs.len()).as_bytes())?;
    for key_spec in spec.key_specs {
        write_spec_map(stream, key_spec)?;
    }
    stream.write_all(format!("*{}\r\n", spec.subcommands.len()).as_bytes())?;
    for subcommand in spec.subcommands {
        write_command_spec(stream, subcommand)?;
    }
    Ok(())
}

/// `COMMAND` / `COMMAND COUNT` / `COMMAND INFO` / `COMMAND DOCS`。
pub(crate) fn handle_command(stream: &mut dyn Write, request: &Request<'_>, protocol: u8) -> io::Result<()> {
    let count = request.arguments.len();
    if count == 1 {
        return write_command_catalog(stream);
    }
    let raw = &request.arguments[1];
    let subcommand = String::from_utf8_lossy(raw).to_ascii_lowercase();
    match subcommand.as_str() {
        "count" => {
            if count != 2 {
                return write_arity(stream, "command|count");
            }
            stream.write_all(format!(":{}\r\n", COMMAND_SPECS.len()).as_bytes())
        }
        "info" => {
            if count == 2 {
                return write_command_catalog(stream);
            }
            stream.write_all(format!("*{}\r\n", count - 2).as_bytes())?;
            for raw in &request.arguments[2..] {
                match lookup_command(raw) {
                    Some(spec) => write_command_spec(stream, spec)?,
                    None => write_null(stream, protocol)?,
                }
            }
            Ok(())
        }
        "docs" => {
            let names: Vec<&'static str> = if count == 2 {
                COMMAND_DOCS.iter().map(|(name, _)| *name).collect()
            }
            else {
                request.arguments[2..]
                    .iter()
                    .filter_map(|raw| {
                        let lowered = String::from_utf8_lossy(raw).to_ascii_lowercase();
                        COMMAND_DOCS.iter().find(|(known, _)| *known == lowered).map(|(known, _)| *known)
                    })
                    .collect()
            };
            stream.write_all(format!("*{}\r\n", names.len() * 2).as_bytes())?;
            for name in names {
                write_bulk(stream, name.as_bytes())?;
                let docs = COMMAND_DOCS.iter().find(|(known, _)| *known == name).map(|(_, docs)| *docs).unwrap_or(&[]);
                write_docs_value(stream, &DocsValue::List(docs))?;
            }
            Ok(())
        }
        _ => write_error(stream, &format!("unknown subcommand '{}'. Try COMMAND HELP.", String::from_utf8_lossy(raw))),
    }
}

fn write_command_catalog(stream: &mut dyn Write) -> io::Result<()> {
    stream.write_all(format!("*{}\r\n", COMMAND_SPECS.len()).as_bytes())?;
    for spec in COMMAND_SPECS {
        write_command_spec(stream, spec)?;
    }
    Ok(())
}

fn lookup_command(name: &[u8]) -> Option<&'static CommandSpec> {
    let lowered = String::from_utf8_lossy(name).to_ascii_lowercase();
    COMMAND_SPECS.iter().find(|spec| spec.name == lowered)
}

fn write_docs_value(stream: &mut dyn Write, value: &DocsValue) -> io::Result<()> {
    match value {
        DocsValue::Text(text) => write_bulk(stream, text.as_bytes()),
        DocsValue::Int(number) => stream.write_all(format!(":{number}\r\n").as_bytes()),
        DocsValue::List(values) => {
            stream.write_all(format!("*{}\r\n", values.len()).as_bytes())?;
            for value in values.iter() {
                write_docs_value(stream, value)?;
            }
            Ok(())
        }
    }
}

/// `COMMAND DOCS` 的值：文本、整数或由它们组成的数组。
enum DocsValue {
    Text(&'static str),
    Int(i64),
    List(&'static [DocsValue]),
}

// ---------------------------------------------------------------------------
// `INFO`
// ---------------------------------------------------------------------------

/// `INFO [section ...]`，五节顺序与真机一致。
pub(crate) fn handle_info(
    stream: &mut dyn Write,
    request: &Request<'_>,
    registry: &ClientRegistry,
    pubsub: &PubSubState,
    executor: Option<&dyn KeyValueExecutor>,
) -> io::Result<()> {
    let requested: Vec<String> = request.arguments[1..].iter().map(|raw| String::from_utf8_lossy(raw).to_ascii_lowercase()).collect();
    let all = requested.is_empty() || requested.iter().any(|name| matches!(name.as_str(), "all" | "everything" | "default"));
    let mut parts = String::new();
    for section in ["server", "clients", "memory", "stats", "keyspace"] {
        if !all && !requested.iter().any(|name| name == section) {
            continue;
        }
        parts.push_str(&match section {
            "server" => info_server(registry),
            "clients" => info_clients(registry, pubsub),
            "memory" => info_memory(),
            "stats" => info_stats(registry, pubsub),
            _ => info_keyspace(executor),
        });
    }
    write_bulk(stream, parts.as_bytes())
}

/// Node 侧的 `process.platform` 取值，`INFO server` 的 `os:` 行要和 TS 服务端同形。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// Node 侧的 `process.arch` 取值，与 {@link node_platform} 一起拼成 `os:` 行。
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

fn info_server(registry: &ClientRegistry) -> String {
    let uptime = registry.started.elapsed().as_secs();
    let now = now_unix_ms();
    [
        "# Server".to_string(),
        "redis_version:7.4.0".into(),
        "redis_git_sha1:00000000".into(),
        "redis_git_dirty:0".into(),
        "redis_build_id:0".into(),
        "redis_mode:standalone".into(),
        format!("os:{} {}", node_platform(), node_arch()),
        "arch_bits:64".into(),
        "monotonic_clock:POSIX clock_gettime".into(),
        "multiplexing_api:epoll".into(),
        "atomicvar_api:c11-builtin".into(),
        format!("process_id:{}", std::process::id()),
        "process_supervised:no".into(),
        format!("run_id:{}", registry.run_id),
        format!("tcp_port:{}", registry.port),
        format!("server_time_usec:{}", now.saturating_mul(1000)),
        format!("uptime_in_seconds:{uptime}"),
        format!("uptime_in_days:{}", uptime / 86400),
        "hz:10".into(),
        "configured_hz:10".into(),
        format!("lru_clock:{}", (now / 1000) % 1_073_741_824),
        "executable:".into(),
        "config_file:".into(),
        "io_threads_active:0".into(),
        format!("listener0:name=tcp,bind={},port={}", registry.bind, registry.port),
        String::new(),
    ]
    .join("\r\n")
}

fn info_clients(registry: &ClientRegistry, pubsub: &PubSubState) -> String {
    let clients = registry.count();
    let entries = registry.snapshot();
    let mut subscribers = 0;
    for entry in &entries {
        let (sub, psub) = pubsub.counts(entry.pubsub_id);
        if sub + psub > 0 {
            subscribers += 1;
        }
    }
    [
        "# Clients".to_string(),
        format!("connected_clients:{clients}"),
        "cluster_connections:0".into(),
        "maxclients:10000".into(),
        "client_recent_max_input_buffer:0".into(),
        "client_recent_max_output_buffer:0".into(),
        "blocked_clients:0".into(),
        "tracking_clients:0".into(),
        format!("pubsub_clients:{subscribers}"),
        "watching_clients:0".into(),
        "clients_in_timeout_table:0".into(),
        format!("active_clients:{clients}"),
        "total_watched_keys:0".into(),
        "total_blocking_keys:0".into(),
        "total_blocking_keys_on_nokey:0".into(),
        String::new(),
    ]
    .join("\r\n")
}

fn info_memory() -> String {
    [
        "# Memory".to_string(),
        "used_memory:0".into(),
        "used_memory_human:0.00B".into(),
        "used_memory_rss:0".into(),
        "used_memory_rss_human:0.00B".into(),
        "used_memory_peak:0".into(),
        "used_memory_peak_human:0.00B".into(),
        "used_memory_peak_perc:100.00%".into(),
        "used_memory_overhead:0".into(),
        "used_memory_startup:0".into(),
        "used_memory_dataset:0".into(),
        "used_memory_dataset_perc:0.00%".into(),
        "allocator_allocated:0".into(),
        "allocator_active:0".into(),
        "allocator_resident:0".into(),
        "total_system_memory:0".into(),
        "total_system_memory_human:0.00B".into(),
        "used_memory_lua:0".into(),
        "used_memory_vm_eval:0".into(),
        "used_memory_scripts_eval:0".into(),
        "number_of_cached_scripts:0".into(),
        "number_of_functions:0".into(),
        "number_of_libraries:0".into(),
        "used_memory_vm_functions:0".into(),
        "used_memory_vm_total:0".into(),
        "used_memory_functions:0".into(),
        "used_memory_scripts:0".into(),
        "used_memory_scripts_human:0.00B".into(),
        "maxmemory:0".into(),
        "maxmemory_human:0.00B".into(),
        "maxmemory_policy:noeviction".into(),
        "mem_fragmentation_ratio:1.00".into(),
        "mem_fragmentation_bytes:0".into(),
        "mem_not_counted_for_evict:0".into(),
        "mem_replication_backlog:0".into(),
        "mem_total_replication_buffers:0".into(),
        "mem_clients_slaves:0".into(),
        "mem_clients_normal:0".into(),
        "mem_cluster_links:0".into(),
        "mem_aof_buffer:0".into(),
        "mem_allocator:libc".into(),
        "active_defrag_running:0".into(),
        "lazyfree_pending_objects:0".into(),
        "lazyfree_objects_pending:0".into(),
        "lazyfreed_objects:0".into(),
        String::new(),
    ]
    .join("\r\n")
}

fn info_stats(registry: &ClientRegistry, pubsub: &PubSubState) -> String {
    [
        "# Stats".to_string(),
        format!("total_connections_received:{}", registry.connections.load(Ordering::Acquire)),
        format!("total_commands_processed:{}", registry.commands.load(Ordering::Acquire)),
        "instantaneous_ops_per_sec:0".into(),
        format!("total_net_input_bytes:{}", registry.net_input.load(Ordering::Acquire)),
        "total_net_output_bytes:0".into(),
        "total_net_repl_input_bytes:0".into(),
        "total_net_repl_output_bytes:0".into(),
        "instantaneous_input_kbps:0.00".into(),
        "instantaneous_output_kbps:0.00".into(),
        "rejected_connections:0".into(),
        "sync_full:0".into(),
        "sync_partial_ok:0".into(),
        "sync_partial_err:0".into(),
        "expired_keys:0".into(),
        "expired_subkeys:0".into(),
        "evicted_keys:0".into(),
        "evicted_subkeys:0".into(),
        "keyspace_hits:0".into(),
        "keyspace_misses:0".into(),
        format!("pubsub_channels:{}", pubsub.channels(None).len()),
        format!("pubsub_patterns:{}", pubsub.numpat()),
        "pubsubshard_channels:0".into(),
        "latest_fork_usec:0".into(),
        "total_forks:0".into(),
        "migrate_cached_sockets:0".into(),
        "slave_expires_count:0".into(),
        "used_cpu_sys:0.000000".into(),
        "used_cpu_user:0.000000".into(),
        "used_cpu_sys_children:0.000000".into(),
        "used_cpu_user_children:0.000000".into(),
        format!("total_reads_processed:{}", registry.reads.load(Ordering::Acquire)),
        format!("total_writes_processed:{}", registry.commands.load(Ordering::Acquire)),
        "io_threaded_reads_processed:0".into(),
        "io_threaded_writes_processed:0".into(),
        "reply_buffer_shrinks:0".into(),
        "reply_buffer_expands:0".into(),
        "eventloop_cycles:0".into(),
        "eventloop_duration_sum:0".into(),
        "eventloop_duration_cmd_sum:0".into(),
        "instantaneous_eventloop_cycles_per_sec:0".into(),
        "instantaneous_eventloop_duration_usec:0".into(),
        "tracking_total_keys:0".into(),
        "tracking_total_items:0".into(),
        "tracking_total_prefixes:0".into(),
        "unexpected_error_replies:0".into(),
        "total_error_replies:0".into(),
        "total_eviction_exceeded_time:0".into(),
        "current_eviction_exceeded_time:0".into(),
        String::new(),
    ]
    .join("\r\n")
}

fn info_keyspace(executor: Option<&dyn KeyValueExecutor>) -> String {
    let mut lines = vec!["# Keyspace".to_string()];
    if let Some(executor) = executor {
        for count in strings::keyspace_stats(executor) {
            lines.push(format!("db{}:keys={},expires={},avg_ttl=0,subexpiry=0", count.database, count.keys, count.expires));
        }
    }
    lines.push(String::new());
    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// `CONFIG`
// ---------------------------------------------------------------------------

/// `CONFIG GET` 的默认值，顺序与 TS 侧一致（真机 GET 也按注册顺序返回）。
const CONFIG_DEFAULTS: &[(&str, &str)] = &[
    ("maxmemory", "0"),
    ("maxmemory-policy", "noeviction"),
    ("maxmemory-samples", "5"),
    ("maxmemory-eviction-tenacity", "10"),
    ("maxmemory-clients", "0"),
    ("save", ""),
    ("appendonly", "no"),
    ("appendfsync", "everysec"),
    ("requirepass", ""),
    ("notify-keyspace-events", ""),
    ("timeout", "0"),
    ("tcp-keepalive", "300"),
    ("databases", "16"),
    ("bind", "127.0.0.1"),
    ("port", "0"),
    ("protected-mode", "yes"),
    ("loglevel", "notice"),
    ("slowlog-log-slower-than", "10000"),
    ("slowlog-max-len", "128"),
];

const CONFIG_IMMUTABLE: &[&str] = &["databases", "port", "supervised"];

const MAXMEMORY_POLICIES: &[&str] = &[
    "volatile-lru",
    "volatile-lfu",
    "volatile-random",
    "volatile-ttl",
    "volatile-lrm",
    "allkeys-lru",
    "allkeys-lfu",
    "allkeys-random",
    "allkeys-lrm",
    "noeviction",
];

/// 由网关生效、但宿主运行时仍需同步的选项。
///
/// `requirepass` 的认证判定、`CONFIG GET` 的取值与校验全部留在网关；宿主只在自己
/// 的数据路径上镜像 `appendfsync`、通知标志与 slow log 阈值。
const HOST_EFFECT_OPTIONS: &[&str] = &["appendfsync", "requirepass", "notify-keyspace-events", "slowlog-log-slower-than", "slowlog-max-len"];

/// `CONFIG GET pattern [pattern ...]` 与 `CONFIG SET name value [name value ...]`。
///
/// 返回本次生效且宿主需要镜像的选项；`CONFIG GET` 与失败分支返回空表。
pub(crate) fn handle_config(stream: &mut dyn Write, request: &Request<'_>, registry: &ClientRegistry) -> io::Result<Vec<(String, String)>> {
    let count = request.arguments.len();
    if count < 2 {
        return write_arity(stream, "config").map(|()| Vec::new());
    }
    let raw = &request.arguments[1];
    let subcommand = String::from_utf8_lossy(raw).to_ascii_lowercase();
    match subcommand.as_str() {
        "get" => {
            if count < 3 {
                return write_arity(stream, "config|get").map(|()| Vec::new());
            }
            let values = registry.config.lock().unwrap_or_else(|poison| poison.into_inner()).clone();
            let mut matched: Vec<(&'static str, String)> = Vec::new();
            for pattern in &request.arguments[2..] {
                for (name, value) in &values {
                    if glob_match(pattern, name.as_bytes()) && !matched.iter().any(|(known, _)| known == name) {
                        matched.push((*name, value.clone()));
                    }
                }
            }
            stream.write_all(format!("*{}\r\n", matched.len() * 2).as_bytes())?;
            for (name, value) in matched {
                write_bulk(stream, name.as_bytes())?;
                write_bulk(stream, value.as_bytes())?;
            }
            Ok(Vec::new())
        }
        "set" => {
            if count < 4 || count % 2 != 0 {
                write_error(stream, "Unknown option or number of arguments for CONFIG SET")?;
                return Ok(Vec::new());
            }
            let values = registry.config.lock().unwrap_or_else(|poison| poison.into_inner()).clone();
            let mut pairs: Vec<(&'static str, String)> = Vec::new();
            for chunk in request.arguments[2..].chunks(2) {
                let raw_name = chunk[0];
                let lowered = String::from_utf8_lossy(raw_name).to_ascii_lowercase();
                let Some((name, _)) = values.iter().find(|(name, _)| *name == lowered)
                else {
                    write_error(
                        stream,
                        &format!("Unknown option or number of arguments for CONFIG SET - '{}'", String::from_utf8_lossy(raw_name)),
                    )?;
                    return Ok(Vec::new());
                };
                let value = String::from_utf8_lossy(chunk[1]).into_owned();
                if let Some(message) = validate_config_set(*name, &value) {
                    write_error(stream, &message)?;
                    return Ok(Vec::new());
                }
                pairs.push((*name, value));
            }
            let mut applied = Vec::new();
            let mut values = registry.config.lock().unwrap_or_else(|poison| poison.into_inner());
            for (name, value) in pairs {
                let stored =
                    if name == "maxmemory" { parse_memory_value(&value).map(|bytes| bytes.to_string()).unwrap_or(value) } else { value };
                if let Some(entry) = values.iter_mut().find(|(known, _)| *known == name) {
                    entry.1 = stored.clone();
                }
                if name == "requirepass" {
                    // 认证判定读取的就是这里写回的口令，宿主不再需要第二份。
                    registry.set_requirepass(if stored.is_empty() { None } else { Some(stored.clone()) });
                }
                if HOST_EFFECT_OPTIONS.contains(&name) {
                    applied.push((name.to_owned(), stored));
                }
            }
            drop(values);
            stream.write_all(b"+OK\r\n")?;
            Ok(applied)
        }
        _ => write_error(stream, &format!("unknown subcommand '{}'. Try CONFIG HELP.", String::from_utf8_lossy(raw))).map(|()| Vec::new()),
    }
}

/// 校验单个 `CONFIG SET` 值，返回真机文案（不含 `ERR ` 前缀）。
fn validate_config_set(name: &str, value: &str) -> Option<String> {
    let failed = |reason: &str| Some(format!("CONFIG SET failed (possibly related to argument '{name}') - {reason}"));
    if CONFIG_IMMUTABLE.contains(&name) {
        return failed("can't set immutable config");
    }
    match name {
        "maxmemory" => {
            if parse_memory_value(value).is_none() {
                return failed("argument must be a memory value");
            }
        }
        "maxmemory-policy" => {
            if !MAXMEMORY_POLICIES.contains(&value.to_ascii_lowercase().as_str()) {
                return failed(&format!("argument(s) must be one of the following: {}", MAXMEMORY_POLICIES.join(", ")));
            }
        }
        "appendonly" => {
            let lowered = value.to_ascii_lowercase();
            if lowered != "yes" && lowered != "no" {
                return failed("argument must be 'yes' or 'no'");
            }
        }
        "appendfsync" => {
            if value != "always" && value != "everysec" {
                return failed("argument(s) must be one of the following: always, everysec");
            }
        }
        "notify-keyspace-events" => {
            if value.chars().any(|character| !"AKEg$lshzxetmdn".contains(character)) {
                return failed("Invalid event class character. Some possible classes are: 'g$lshzxeKE'");
            }
        }
        "slowlog-log-slower-than" => {
            if parse_integer_text(value).is_none_or(|parsed| parsed < -1) {
                return failed("argument couldn't be parsed into an integer");
            }
        }
        "timeout" | "maxmemory-samples" | "slowlog-max-len" => {
            if parse_integer_text(value).is_none_or(|parsed| parsed < 0) {
                return failed("argument couldn't be parsed into an integer");
            }
        }
        "save" => {
            if !value.is_empty() && !valid_save_parameters(value) {
                return failed("Invalid save parameters");
            }
        }
        _ => {}
    }
    None
}

/// `save` 只接受空串或若干「秒 变更数」对。
fn valid_save_parameters(value: &str) -> bool {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() % 2 != 0 {
        return false;
    }
    tokens.iter().all(|token| token.chars().all(|character| character.is_ascii_digit()))
}

/// 解析 `100mb` / `1gb` / 纯字节数，返回字节数。
fn parse_memory_value(raw: &str) -> Option<u64> {
    let value = raw.trim().to_ascii_lowercase();
    let split = value.find(|character: char| !character.is_ascii_digit()).unwrap_or(value.len());
    let (digits, suffix) = value.split_at(split);
    if digits.is_empty() {
        return None;
    }
    let amount = digits.parse::<u64>().ok()?;
    let factor = match suffix {
        "" | "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024 * 1024,
        "g" | "gb" => 1024 * 1024 * 1024,
        "t" | "tb" => 1024_u64.pow(4),
        _ => return None,
    };
    amount.checked_mul(factor)
}

// ---------------------------------------------------------------------------
// `ACL`
// ---------------------------------------------------------------------------

const ACL_CATEGORIES: &[&str] = &[
    "keyspace",
    "read",
    "write",
    "set",
    "sortedset",
    "list",
    "hash",
    "string",
    "array",
    "bitmap",
    "hyperloglog",
    "geo",
    "stream",
    "pubsub",
    "admin",
    "fast",
    "slow",
    "blocking",
    "dangerous",
    "connection",
    "transaction",
    "scripting",
];

/// `ACL WHOAMI | LIST | CAT [category] | GETUSER user`，只有 `default` 用户。
pub(crate) fn handle_acl(stream: &mut dyn Write, request: &Request<'_>, protocol: u8) -> io::Result<()> {
    let count = request.arguments.len();
    if count < 2 {
        return write_arity(stream, "acl");
    }
    let raw = &request.arguments[1];
    let subcommand = String::from_utf8_lossy(raw).to_ascii_lowercase();
    match subcommand.as_str() {
        "whoami" => {
            if count != 2 {
                return write_arity(stream, "acl|whoami");
            }
            write_bulk(stream, b"default")
        }
        "list" => {
            if count != 2 {
                return write_arity(stream, "acl|list");
            }
            stream.write_all(b"*1\r\n")?;
            write_bulk(stream, b"user default on nopass sanitize-payload ~* &* +@all")
        }
        "cat" => {
            if count == 2 {
                stream.write_all(format!("*{}\r\n", ACL_CATEGORIES.len()).as_bytes())?;
                for category in ACL_CATEGORIES {
                    write_bulk(stream, category.as_bytes())?;
                }
                return Ok(());
            }
            if count != 3 {
                write_error(stream, "unknown subcommand or wrong number of arguments for 'CAT'. Try ACL HELP.")?;
                return Ok(());
            }
            let category = String::from_utf8_lossy(request.arguments[2]).to_ascii_lowercase();
            if !ACL_CATEGORIES.contains(&category.as_str()) {
                write_error(stream, &format!("Unknown category '{}'", String::from_utf8_lossy(request.arguments[2])))?;
                return Ok(());
            }
            let marker = format!("@{category}");
            let names: Vec<&str> = COMMAND_SPECS.iter().filter(|spec| spec.acl.contains(&marker.as_str())).map(|spec| spec.name).collect();
            stream.write_all(format!("*{}\r\n", names.len()).as_bytes())?;
            for name in names {
                write_bulk(stream, name.as_bytes())?;
            }
            Ok(())
        }
        "getuser" => {
            if count != 3 {
                return write_arity(stream, "acl|getuser");
            }
            if !String::from_utf8_lossy(request.arguments[2]).eq_ignore_ascii_case("default") {
                return write_null(stream, protocol);
            }
            stream.write_all(b"*12\r\n")?;
            write_bulk(stream, b"flags")?;
            stream.write_all(b"*3\r\n")?;
            for flag in ["on", "nopass", "sanitize-payload"] {
                write_bulk(stream, flag.as_bytes())?;
            }
            write_bulk(stream, b"passwords")?;
            stream.write_all(b"*0\r\n")?;
            write_bulk(stream, b"commands")?;
            write_bulk(stream, b"+@all")?;
            write_bulk(stream, b"keys")?;
            write_bulk(stream, b"~*")?;
            write_bulk(stream, b"channels")?;
            write_bulk(stream, b"&*")?;
            write_bulk(stream, b"selectors")?;
            stream.write_all(b"*0\r\n")
        }
        _ => write_error(stream, &format!("unknown subcommand '{}'. Try ACL HELP.", String::from_utf8_lossy(raw))),
    }
}

/// 嵌入式服务器面会话，复用监听器同一套 `CLIENT` / `COMMAND` / `INFO` / `CONFIG` / `ACL`。
///
/// 监听器用真实 socket 登记连接；语言绑定没有 socket，因此这里按宿主给定的连接号登记
/// 一条无写侧的记录。多条嵌入式会话共享同一个 [`ClientRegistry`]，`CLIENT LIST` 与
/// `CLIENT KILL` 因此能看到并关闭宿主侧的其它连接。
pub struct EmbeddedServerFace {
    registry: Arc<ClientRegistry>,
    pubsub: Arc<PubSubState>,
    state: ConnectionState,
    /// 这条连接的认证状态机（`AUTH` / `NOAUTH`）。
    auth: AuthState,
    /// 当前逻辑库号，`WATCH` 取修订号时据此构造命名空间。
    database: u8,
}

impl EmbeddedServerFace {
    /// 创建一条独立会话，配置取真机默认值。
    pub fn new() -> Self {
        Self::with_id(Arc::new(ClientRegistry::standalone()), 1)
    }

    /// 在共享登记表里用宿主连接号创建一条会话。
    pub fn with_id(registry: Arc<ClientRegistry>, id: u64) -> Self {
        Self::with_id_and_pubsub(registry, id, Arc::new(PubSubState::new()))
    }

    /// 在共享登记表与共享频道表里用宿主连接号创建一条会话。
    ///
    /// 绑定层把同一张 [`PubSubState`] 交给同进程的每条会话，`CLIENT LIST` 与
    /// `INFO` 因此读到宿主侧的订阅计数，而不只是本会话自己的。
    pub fn with_id_and_pubsub(registry: Arc<ClientRegistry>, id: u64, pubsub: Arc<PubSubState>) -> Self {
        let state = ConnectionState::new(registry.attach_detached_with(id));
        let requirepass = registry.requirepass();
        Self { registry, pubsub, state, auth: AuthState::new(requirepass.as_deref()), database: 0 }
    }

    /// 同步宿主会话的库号与 RESP 版本，`CLIENT LIST` 与 `COMMAND` 读取它们。
    pub fn set_session(&mut self, database: u8, protocol: u8) {
        self.state.protocol = protocol;
        self.state.record.set_database(database);
        self.state.record.set_protocol(protocol);
        self.database = database;
    }

    /// 宿主为它自己拥有的命令族排队一条命令，返回队列长度。
    ///
    /// 排队期校验归命令族的 owner：上游能校验的族由 [`execute`](Self::execute) 直接
    /// 入队，宿主实现的族（streams、`EVAL` 等）在宿主自己校验之后走这里，队列本体
    /// 始终只存在上游会话里。
    pub fn queue_host_command(&mut self, arguments: &[&[u8]]) -> usize {
        let queued = self.state.session.queue(arguments);
        self.state.record.set_multi(Some(queued));
        queued
    }

    /// 宿主在排队期拒绝一条命令后给事务中毒。
    pub fn poison_host_command(&mut self) {
        self.state.session.poison();
    }

    /// 当前是否处于 `MULTI` 块内。
    pub fn in_multi(&self) -> bool {
        self.state.session.in_multi()
    }

    /// 嵌入式会话网关：执行一条命令并返回宿主必须跟进的副作用。
    ///
    /// 网关拥有连接状态机的全部判定：`NOAUTH` 门禁、`AUTH` / `HELLO` / `RESET` /
    /// `QUIT`、`MULTI` 排队与中毒、`WATCH` 修订号、`EXEC` 的放弃判定、持久化计划与
    /// `WAIT` / `LATENCY`。命令语义属于宿主的族（数据族、`CONFIG` 的本地选项、
    /// streams）返回 `handled = false`，由宿主自己运行或排队。`replaying` 表示这条
    /// 命令来自 `EXEC` 复放：`WAIT` 立即回 0，与真机在事务内不复挂起的语义一致。
    pub fn execute(
        &mut self,
        stream: &mut dyn Write,
        arguments: &[&[u8]],
        executor: &dyn KeyValueExecutor,
        replaying: bool,
    ) -> io::Result<SessionOutcome> {
        let mut outcome = SessionOutcome::default();
        outcome.in_multi = self.state.session.in_multi();
        let request = Request { arguments: arguments.to_vec(), consumed: 0 };
        let command = arguments.first().copied().unwrap_or_default();
        let count = arguments.len();
        self.state.record.touch(command_context(command, arguments));
        self.registry.bump_command();
        if command.is_empty() {
            outcome.handled = true;
            return Ok(outcome);
        }
        if let Some(reply) = self.auth.gate(command, self.registry.requirepass().as_deref()) {
            self.answer(stream, &mut outcome, reply)?;
            return Ok(outcome);
        }
        // RESP2 订阅模式：只有 pub/sub 动词、`PING`、`QUIT` 与 `RESET` 可以执行。
        // 订阅表由网关持有，宿主不再自判准入。
        if self.state.protocol == 2
            && self.pubsub.is_subscribed(self.state.record.id)
            && !crate::yyds::redis::connection::subscriber_command_allowed(command)
        {
            let message = subscriber_gate_error(command, arguments);
            self.answer_error(stream, &mut outcome, CommandError::Coded(message))?;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"QUIT") {
            self.answer(stream, &mut outcome, Reply::ok())?;
            outcome.close = true;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"AUTH") {
            let reply = self.auth.authenticate(arguments, self.registry.requirepass().as_deref());
            self.answer(stream, &mut outcome, reply)?;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"HELLO") {
            return self.handle_hello(stream, arguments, outcome);
        }
        if command.eq_ignore_ascii_case(b"RESET") {
            if count != 1 {
                self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'reset' command".to_owned()))?;
                return Ok(outcome);
            }
            self.state.session.discard();
            self.state.record.set_multi(None);
            self.state.protocol = 2;
            self.state.record.set_protocol(2);
            self.database = 0;
            self.state.record.set_database(0);
            self.answer(stream, &mut outcome, Reply::status("RESET"))?;
            outcome.reset = true;
            outcome.protocol = Some(2);
            outcome.in_multi = false;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"WATCH") {
            return self.handle_watch(stream, arguments, executor, outcome);
        }
        if command.eq_ignore_ascii_case(b"MULTI") {
            if count != 1 {
                self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'multi' command".to_owned()))?;
                return Ok(outcome);
            }
            if self.state.session.in_multi() {
                self.answer_error(stream, &mut outcome, CommandError::Failed("MULTI calls can not be nested".to_owned()))?;
                return Ok(outcome);
            }
            self.state.session.begin_multi();
            self.state.record.set_multi(Some(0));
            self.answer(stream, &mut outcome, Reply::ok())?;
            outcome.in_multi = true;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"EXEC") {
            return self.handle_exec(stream, arguments, executor, outcome);
        }
        if command.eq_ignore_ascii_case(b"DISCARD") {
            if count != 1 {
                self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'discard' command".to_owned()))?;
                return Ok(outcome);
            }
            if !self.state.session.in_multi() {
                self.answer_error(stream, &mut outcome, CommandError::Failed("DISCARD without MULTI".to_owned()))?;
                return Ok(outcome);
            }
            self.state.session.discard();
            self.state.record.set_multi(None);
            self.answer(stream, &mut outcome, Reply::ok())?;
            outcome.in_multi = false;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"UNWATCH") {
            if count != 1 {
                self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'unwatch' command".to_owned()))?;
                return Ok(outcome);
            }
            if self.state.session.in_multi() {
                self.queue(stream, &mut outcome, arguments)?;
                return Ok(outcome);
            }
            self.state.session.unwatch();
            self.answer(stream, &mut outcome, Reply::ok())?;
            return Ok(outcome);
        }
        // `SAVE` 与 `SHUTDOWN` 是 no-multi 命令：排队尝试直接毒化整个事务。
        if self.state.session.in_multi() && (command.eq_ignore_ascii_case(b"SAVE") || command.eq_ignore_ascii_case(b"SHUTDOWN")) {
            self.state.session.poison();
            let upper = String::from_utf8_lossy(command).to_uppercase();
            self.answer_error(stream, &mut outcome, CommandError::Coded(format!("ERR Command {upper} can not be used in MULTI")))?;
            return Ok(outcome);
        }
        if PersistenceState::is_command(command) {
            return self.handle_persistence(stream, arguments, outcome);
        }
        if command.eq_ignore_ascii_case(b"WAIT") {
            return self.handle_wait(stream, arguments, outcome, replaying);
        }
        if command.eq_ignore_ascii_case(b"LATENCY") {
            return self.handle_latency(stream, arguments, outcome);
        }
        if self.state.session.in_multi() {
            // 上游能校验的族在这里入队；宿主拥有的族以 `handled = false` 落回，
            // 由宿主自行校验后经 `queue_host_command` 入队。
            return match session::validate_command_for_queue(command, count, arguments, true, &self.namespace()) {
                Ok(()) => {
                    self.queue(stream, &mut outcome, arguments)?;
                    Ok(outcome)
                }
                Err(error) if is_unsupported(&error) => Ok(outcome),
                Err(error) => {
                    self.state.session.poison();
                    self.answer_error(stream, &mut outcome, error)?;
                    Ok(outcome)
                }
            };
        }
        if command.eq_ignore_ascii_case(b"CLIENT") {
            let alive = handle_client(stream, &request, &self.state, &self.registry, &self.pubsub)?;
            outcome.handled = true;
            outcome.close = !alive;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"COMMAND") {
            handle_command(stream, &request, self.state.protocol)?;
            outcome.handled = true;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"INFO") {
            handle_info(stream, &request, &self.registry, &self.pubsub, Some(executor))?;
            outcome.handled = true;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"CONFIG") {
            let applied = handle_config(stream, &request, &self.registry)?;
            outcome.config = applied.into_iter().map(|(name, value)| ConfigChange { name, value }).collect();
            outcome.handled = true;
            return Ok(outcome);
        }
        if command.eq_ignore_ascii_case(b"ACL") {
            handle_acl(stream, &request, self.state.protocol)?;
            outcome.handled = true;
            return Ok(outcome);
        }
        // 数据族与其余宿主语义：网关不执行、不代答。
        Ok(outcome)
    }

    fn handle_hello(&mut self, stream: &mut dyn Write, arguments: &[&[u8]], mut outcome: SessionOutcome) -> io::Result<SessionOutcome> {
        if arguments.len() == 1 {
            self.write_hello(stream)?;
            outcome.handled = true;
            return Ok(outcome);
        }
        let protocol = match session::parse_integer(arguments[1]) {
            Some(value @ (2 | 3)) => value as u8,
            _ => {
                self.answer_error(stream, &mut outcome, CommandError::Coded("NOPROTO unsupported protocol version".to_owned()))?;
                return Ok(outcome);
            }
        };
        let mut username: Option<String> = None;
        let mut password: Option<String> = None;
        let mut name: Option<Vec<u8>> = None;
        let mut index = 2;
        while index < arguments.len() {
            let option = String::from_utf8_lossy(arguments[index]).to_uppercase();
            if option == "AUTH" {
                if username.is_some() || password.is_some() || index + 2 >= arguments.len() {
                    self.answer_error(stream, &mut outcome, CommandError::Failed("syntax error".to_owned()))?;
                    return Ok(outcome);
                }
                username = Some(String::from_utf8_lossy(arguments[index + 1]).into_owned());
                password = Some(String::from_utf8_lossy(arguments[index + 2]).into_owned());
                index += 3;
            }
            else if option == "SETNAME" {
                if name.is_some() || index + 1 >= arguments.len() {
                    self.answer_error(stream, &mut outcome, CommandError::Failed("syntax error".to_owned()))?;
                    return Ok(outcome);
                }
                name = Some(arguments[index + 1].to_vec());
                index += 2;
            }
            else {
                self.answer_error(stream, &mut outcome, CommandError::Failed("syntax error".to_owned()))?;
                return Ok(outcome);
            }
        }
        if let (Some(username), Some(password)) = (&username, &password)
            && let Err(error) = self.auth.check_credential(username, password, false, self.registry.requirepass().as_deref())
        {
            self.answer_error(stream, &mut outcome, error)?;
            return Ok(outcome);
        }
        self.state.protocol = protocol;
        self.state.record.set_protocol(protocol);
        if let Some(name) = name {
            let empty = name.is_empty();
            self.state.record.set_name(if empty { None } else { Some(name.clone()) });
            outcome.name = Some(name);
        }
        self.write_hello(stream)?;
        outcome.handled = true;
        outcome.protocol = Some(protocol);
        Ok(outcome)
    }

    fn handle_watch(
        &mut self,
        stream: &mut dyn Write,
        arguments: &[&[u8]],
        executor: &dyn KeyValueExecutor,
        mut outcome: SessionOutcome,
    ) -> io::Result<SessionOutcome> {
        if arguments.len() < 2 {
            self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'watch' command".to_owned()))?;
            return Ok(outcome);
        }
        if self.state.session.in_multi() {
            self.answer_error(stream, &mut outcome, CommandError::Failed("WATCH inside MULTI is not allowed".to_owned()))?;
            return Ok(outcome);
        }
        let namespace = self.namespace();
        for key in &arguments[1..] {
            if self.state.session.is_watching(&namespace, key) {
                continue;
            }
            let revision = key_revision(executor, &namespace, key).map_err(io::Error::other)?;
            self.state.session.watch(&namespace, key, revision);
        }
        self.answer(stream, &mut outcome, Reply::ok())?;
        Ok(outcome)
    }

    fn handle_exec(
        &mut self,
        stream: &mut dyn Write,
        arguments: &[&[u8]],
        executor: &dyn KeyValueExecutor,
        mut outcome: SessionOutcome,
    ) -> io::Result<SessionOutcome> {
        if arguments.len() != 1 {
            self.answer_error(stream, &mut outcome, CommandError::Failed("wrong number of arguments for 'exec' command".to_owned()))?;
            return Ok(outcome);
        }
        if !self.state.session.in_multi() {
            self.answer_error(stream, &mut outcome, CommandError::Failed("EXEC without MULTI".to_owned()))?;
            return Ok(outcome);
        }
        let dirty = self.state.session.multi_error();
        let watched = self.state.session.watched().to_vec();
        let queue = self.state.session.finish_exec();
        self.state.record.set_multi(None);
        outcome.in_multi = false;
        if dirty {
            self.answer_error(
                stream,
                &mut outcome,
                CommandError::Coded("EXECABORT Transaction discarded because of previous errors.".to_owned()),
            )?;
            return Ok(outcome);
        }
        if !watched.is_empty() && watched_keys_modified(executor, &watched).map_err(io::Error::other)? {
            self.answer(stream, &mut outcome, Reply::NullArray)?;
            return Ok(outcome);
        }
        if queue.is_empty() {
            self.answer(stream, &mut outcome, Reply::array(Vec::new()))?;
            return Ok(outcome);
        }
        outcome.handled = true;
        outcome.replay = queue.into_iter().map(|entry| entry.request.into_arguments()).collect();
        Ok(outcome)
    }

    fn handle_persistence(&mut self, stream: &mut dyn Write, arguments: &[&[u8]], mut outcome: SessionOutcome) -> io::Result<SessionOutcome> {
        let command = arguments.first().copied().unwrap_or_default();
        if self.state.session.in_multi() {
            if let Err(error) = PersistenceState::validate_for_queue(command, arguments) {
                self.state.session.poison();
                self.answer_error(stream, &mut outcome, error)?;
                return Ok(outcome);
            }
            self.queue(stream, &mut outcome, arguments)?;
            return Ok(outcome);
        }
        let now_unix = now_unix_ms() / 1000;
        let planned = self.registry.persistence.lock().unwrap_or_else(|poison| poison.into_inner()).plan(command, arguments, now_unix);
        if let Some(reply) = planned.reply {
            self.answer(stream, &mut outcome, reply)?;
        }
        else {
            outcome.handled = true;
        }
        outcome.action = planned.action;
        if matches!(planned.action, PersistenceAction::Shutdown { .. }) {
            outcome.close = true;
        }
        Ok(outcome)
    }

    fn handle_wait(
        &mut self,
        stream: &mut dyn Write,
        arguments: &[&[u8]],
        mut outcome: SessionOutcome,
        replaying: bool,
    ) -> io::Result<SessionOutcome> {
        match introspection::plan_wait(arguments) {
            Ok(WaitPlan::Immediate(reply)) => {
                self.answer(stream, &mut outcome, reply)?;
                Ok(outcome)
            }
            Ok(WaitPlan::Delay { timeout_ms: _ }) if self.state.session.in_multi() => {
                self.queue(stream, &mut outcome, arguments)?;
                Ok(outcome)
            }
            Ok(WaitPlan::Delay { timeout_ms: _ }) if replaying => {
                self.answer(stream, &mut outcome, Reply::Integer(0))?;
                Ok(outcome)
            }
            Ok(WaitPlan::Delay { timeout_ms }) => {
                outcome.handled = true;
                outcome.wait = Some(timeout_ms);
                Ok(outcome)
            }
            Err(error) => {
                if self.state.session.in_multi() {
                    self.state.session.poison();
                }
                self.answer_error(stream, &mut outcome, error)?;
                Ok(outcome)
            }
        }
    }

    fn handle_latency(&mut self, stream: &mut dyn Write, arguments: &[&[u8]], mut outcome: SessionOutcome) -> io::Result<SessionOutcome> {
        match introspection::plan_latency(arguments) {
            Ok(_) if self.state.session.in_multi() => {
                self.queue(stream, &mut outcome, arguments)?;
                Ok(outcome)
            }
            Ok(reply) => {
                self.answer(stream, &mut outcome, reply)?;
                Ok(outcome)
            }
            Err(error) => {
                if self.state.session.in_multi() {
                    self.state.session.poison();
                }
                self.answer_error(stream, &mut outcome, error)?;
                Ok(outcome)
            }
        }
    }

    fn write_hello(&self, stream: &mut dyn Write) -> io::Result<()> {
        let reply = Reply::Map(vec![
            (Reply::bulk("server".to_owned()), Reply::bulk("redis".to_owned())),
            (Reply::bulk("version".to_owned()), Reply::bulk("7.4.0".to_owned())),
            (Reply::bulk("proto".to_owned()), Reply::Integer(i64::from(self.state.protocol))),
            (Reply::bulk("id".to_owned()), Reply::Integer(self.state.record.id as i64)),
            (Reply::bulk("mode".to_owned()), Reply::bulk("standalone".to_owned())),
            (Reply::bulk("role".to_owned()), Reply::bulk("master".to_owned())),
            (Reply::bulk("modules".to_owned()), Reply::array(Vec::new())),
        ]);
        self.write_reply(stream, &reply)
    }

    fn namespace(&self) -> Namespace {
        Namespace(redis_namespace(self.database))
    }

    fn write_reply(&self, stream: &mut dyn Write, reply: &Reply) -> io::Result<()> {
        let mut frame = Vec::new();
        reply.encode(self.state.protocol, &mut frame);
        stream.write_all(&frame)
    }

    fn answer(&self, stream: &mut dyn Write, outcome: &mut SessionOutcome, reply: Reply) -> io::Result<()> {
        self.write_reply(stream, &reply)?;
        outcome.handled = true;
        Ok(())
    }

    fn answer_error(&self, stream: &mut dyn Write, outcome: &mut SessionOutcome, error: CommandError) -> io::Result<()> {
        self.answer(stream, outcome, error.to_reply())
    }

    fn queue(&mut self, stream: &mut dyn Write, outcome: &mut SessionOutcome, arguments: &[&[u8]]) -> io::Result<()> {
        let queued = self.state.session.queue(arguments);
        self.state.record.set_multi(Some(queued));
        self.answer(stream, outcome, Reply::status("QUEUED"))
    }
}

/// 嵌入式会话网关的副作用集，宿主按字段跟进。
#[derive(Debug, Default)]
pub struct SessionOutcome {
    /// 网关已经答复这条命令，宿主不得再自行运行。
    pub handled: bool,
    /// 连接必须关闭（`QUIT`、`CLIENT KILL` 命中自身或 `SHUTDOWN`）。
    pub close: bool,
    /// 宿主必须对存储执行的持久化动作。
    pub action: PersistenceAction,
    /// `EXEC` 释放的队列，宿主按序逐条复放后回数组。
    pub replay: Vec<Vec<Vec<u8>>>,
    /// `WAIT` 停车指令：宿主挂起连接并在超时后回整数 0。
    pub wait: Option<i64>,
    /// `HELLO` / `RESET` 之后的生效 RESP 版本，供宿主镜像。
    pub protocol: Option<u8>,
    /// `HELLO ... SETNAME` 写入的连接名，供宿主镜像。
    pub name: Option<Vec<u8>>,
    /// `RESET` 已运行，宿主必须清订阅并回到 0 号库。
    pub reset: bool,
    /// 本条命令处理完后连接是否仍处于 `MULTI` 块内。
    pub in_multi: bool,
    /// 本次 `CONFIG SET` 生效、宿主运行时仍需镜像的选项。
    pub config: Vec<ConfigChange>,
}

/// 一条由网关校验并存储、宿主只需镜像到运行时设置的配置项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    /// 选项名，已小写化。
    pub name: String,
    /// 网关已接受并存储的值。
    pub value: String,
}

/// RESP2 订阅模式下的拒绝文案。
///
/// `PUBSUB` 的子命令解析先于订阅限制：未知子命令、缺参和多余参数都按真机文案
/// 优先报出，已知子命令才以 `pubsub|<sub>` 出现在限制文案里。
fn subscriber_gate_error(command: &[u8], arguments: &[&[u8]]) -> String {
    let mut context = String::from_utf8_lossy(command).to_ascii_lowercase();
    if context == "pubsub" {
        if arguments.len() < 2 {
            return "ERR wrong number of arguments for 'pubsub' command".to_owned();
        }
        let subcommand = String::from_utf8_lossy(arguments[1]).to_ascii_lowercase();
        match subcommand.as_str() {
            "channels" => {
                if arguments.len() > 3 {
                    return format!(
                        "ERR unknown subcommand or wrong number of arguments for '{}'. Try PUBSUB HELP.",
                        String::from_utf8_lossy(arguments[1])
                    );
                }
            }
            "numsub" => {}
            "numpat" => {
                if arguments.len() > 2 {
                    return "ERR wrong number of arguments for 'pubsub|numpat' command".to_owned();
                }
            }
            _ => {
                return format!("ERR unknown subcommand '{}'. Try PUBSUB HELP.", String::from_utf8_lossy(arguments[1]));
            }
        }
        context = format!("pubsub|{subcommand}");
    }
    format!("ERR Can't execute '{context}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context")
}

/// 上游校验表覆盖不到的命令族：让宿主自行校验与排队。
fn is_unsupported(error: &CommandError) -> bool {
    matches!(error, CommandError::Failed(message) if message == "unsupported command")
}

impl Default for EmbeddedServerFace {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 静态表
//
// 与 TypeScript 侧 `COMMAND_INFO_DATA` 同表：同一批元数据只能有一份语义，
// 两面对照门逐字节比较 `COMMAND INFO <name>` 的 reply，改动时两侧一起改。
// ---------------------------------------------------------------------------

const KS_RO_ACCESS_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RO", "access"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RO_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RO"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RW_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RW"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RW_UPDATE_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RW", "update"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RW_ACCESS_UPDATE_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RW", "access", "update"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RM_DELETE_RANGE: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RM", "delete"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(-1)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RO_ACCESS_RANGE: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RO", "access"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(-1)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RO_RANGE: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RO"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(-1)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_RW_INSERT_INDEX1: &[(&str, SpecValue)] = &[
    ("flags", SpecValue::Texts(&["RW", "insert"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

const KS_SET: &[(&str, SpecValue)] = &[
    ("notes", SpecValue::Text("RW and ACCESS due to the optional `GET` argument")),
    ("flags", SpecValue::Texts(&["RW", "access", "update", "variable_flags"])),
    ("begin_search", SpecValue::Map(&[("type", SpecValue::Text("index")), ("spec", SpecValue::Map(&[("index", SpecValue::Int(1))]))])),
    (
        "find_keys",
        SpecValue::Map(&[
            ("type", SpecValue::Text("range")),
            ("spec", SpecValue::Map(&[("lastkey", SpecValue::Int(0)), ("keystep", SpecValue::Int(1)), ("limit", SpecValue::Int(0))])),
        ]),
    ),
];

/// 命令目录，顺序与 TS 侧 `COMMAND_INFO_DATA` 相同。
const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: "get",
        arity: 2,
        flags: &["readonly", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@read", "@string", "@fast"],
        tips: &[],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "set",
        arity: -3,
        flags: &["write", "denyoom"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@string", "@slow"],
        tips: &[],
        key_specs: &[KS_SET],
        subcommands: &[],
    },
    CommandSpec {
        name: "del",
        arity: -2,
        flags: &["write"],
        first: 1,
        last: -1,
        step: 1,
        acl: &["@keyspace", "@write", "@slow"],
        tips: &["request_policy:multi_shard", "response_policy:agg_sum"],
        key_specs: &[KS_RM_DELETE_RANGE],
        subcommands: &[],
    },
    CommandSpec {
        name: "exists",
        arity: -2,
        flags: &["readonly", "fast"],
        first: 1,
        last: -1,
        step: 1,
        acl: &["@keyspace", "@read", "@fast"],
        tips: &["request_policy:multi_shard", "response_policy:agg_sum"],
        key_specs: &[KS_RO_RANGE],
        subcommands: &[],
    },
    CommandSpec {
        name: "expire",
        arity: -3,
        flags: &["write", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@keyspace", "@write", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_UPDATE_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "ttl",
        arity: 2,
        flags: &["readonly", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@keyspace", "@read", "@fast"],
        tips: &["nondeterministic_output"],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "type",
        arity: 2,
        flags: &["readonly", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@keyspace", "@read", "@fast"],
        tips: &[],
        key_specs: &[KS_RO_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "keys",
        arity: 2,
        flags: &["readonly"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@keyspace", "@read", "@slow", "@dangerous"],
        tips: &["request_policy:all_shards", "nondeterministic_output_order"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "scan",
        arity: -2,
        flags: &["readonly"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@keyspace", "@read", "@slow"],
        tips: &["nondeterministic_output", "request_policy:special", "response_policy:special"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "ping",
        arity: -1,
        flags: &["fast"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@fast", "@connection"],
        tips: &["request_policy:all_shards", "response_policy:all_succeeded"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "client",
        arity: -2,
        flags: &[],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@slow"],
        tips: &[],
        key_specs: &[],
        subcommands: CLIENT_SUBCOMMANDS,
    },
    CommandSpec {
        name: "command",
        arity: -1,
        flags: &["loading", "stale"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@slow", "@connection"],
        tips: &["nondeterministic_output_order"],
        key_specs: &[],
        subcommands: COMMAND_SUBCOMMANDS,
    },
    CommandSpec {
        name: "info",
        arity: -1,
        flags: &["loading", "stale"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@slow", "@dangerous"],
        tips: &["nondeterministic_output", "request_policy:all_shards", "response_policy:special"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "config",
        arity: -2,
        flags: &[],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@slow"],
        tips: &[],
        key_specs: &[],
        subcommands: CONFIG_SUBCOMMANDS,
    },
    CommandSpec {
        name: "acl",
        arity: -2,
        flags: &[],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@slow"],
        tips: &[],
        key_specs: &[],
        subcommands: ACL_SUBCOMMANDS,
    },
    CommandSpec {
        name: "hset",
        arity: -4,
        flags: &["write", "denyoom", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@hash", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_UPDATE_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "hget",
        arity: 3,
        flags: &["readonly", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@read", "@hash", "@fast"],
        tips: &[],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "lpush",
        arity: -3,
        flags: &["write", "denyoom", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@list", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_INSERT_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "lrange",
        arity: 4,
        flags: &["readonly"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@read", "@list", "@slow"],
        tips: &[],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "sadd",
        arity: -3,
        flags: &["write", "denyoom", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@set", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_INSERT_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "smembers",
        arity: 2,
        flags: &["readonly"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@read", "@set", "@slow"],
        tips: &["nondeterministic_output_order"],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "zadd",
        arity: -4,
        flags: &["write", "denyoom", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@sortedset", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_UPDATE_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "zrange",
        arity: -4,
        flags: &["readonly"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@read", "@sortedset", "@slow"],
        tips: &[],
        key_specs: &[KS_RO_ACCESS_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "incr",
        arity: 2,
        flags: &["write", "denyoom", "fast"],
        first: 1,
        last: 1,
        step: 1,
        acl: &["@write", "@string", "@fast"],
        tips: &[],
        key_specs: &[KS_RW_ACCESS_UPDATE_INDEX1],
        subcommands: &[],
    },
    CommandSpec {
        name: "mget",
        arity: -2,
        flags: &["readonly", "fast"],
        first: 1,
        last: -1,
        step: 1,
        acl: &["@read", "@string", "@fast"],
        tips: &["request_policy:multi_shard"],
        key_specs: &[KS_RO_ACCESS_RANGE],
        subcommands: &[],
    },
    CommandSpec {
        name: "dbsize",
        arity: 1,
        flags: &["readonly", "fast"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@keyspace", "@read", "@fast"],
        tips: &["request_policy:all_shards", "response_policy:agg_sum"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "flushdb",
        arity: -1,
        flags: &["write"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@keyspace", "@write", "@slow", "@dangerous"],
        tips: &["request_policy:all_shards", "response_policy:all_succeeded"],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "select",
        arity: 2,
        flags: &["loading", "stale", "fast"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@fast", "@connection"],
        tips: &[],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "echo",
        arity: 2,
        flags: &["loading", "stale", "fast"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@fast", "@connection"],
        tips: &[],
        key_specs: &[],
        subcommands: &[],
    },
    CommandSpec {
        name: "quit",
        arity: -1,
        flags: &["noscript", "loading", "stale", "fast", "no_auth", "allow_busy"],
        first: 0,
        last: 0,
        step: 0,
        acl: &["@fast", "@connection"],
        tips: &[],
        key_specs: &[],
        subcommands: &[],
    },
];

const fn subcommand(
    name: &'static str,
    arity: i64,
    flags: &'static [&'static str],
    acl: &'static [&'static str],
    tips: &'static [&'static str],
) -> CommandSpec {
    CommandSpec { name, arity, flags, first: 0, last: 0, step: 0, acl, tips, key_specs: &[], subcommands: &[] }
}

const CLIENT_SUBCOMMANDS: &[CommandSpec] = &[
    subcommand(
        "client|list",
        -2,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous", "@connection"],
        &["nondeterministic_output"],
    ),
    subcommand("client|getname", 2, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|unblock", -3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous", "@connection"], &[]),
    subcommand("client|info", 2, &["noscript", "loading", "stale"], &["@slow", "@connection"], &["nondeterministic_output"]),
    subcommand("client|no-evict", 3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous", "@connection"], &[]),
    subcommand(
        "client|setname",
        3,
        &["noscript", "loading", "stale"],
        &["@slow", "@connection"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand("client|unpause", 2, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous", "@connection"], &[]),
    subcommand("client|id", 2, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|kill", -3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous", "@connection"], &[]),
    subcommand("client|no-touch", 3, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|pause", -3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous", "@connection"], &[]),
    subcommand("client|caching", 3, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|getredir", 2, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|help", 2, &["loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand(
        "client|setinfo",
        4,
        &["noscript", "loading", "stale"],
        &["@slow", "@connection"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand("client|reply", 3, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|trackinginfo", 2, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("client|tracking", -3, &["noscript", "loading", "stale"], &["@slow", "@connection"], &[]),
];

const COMMAND_SUBCOMMANDS: &[CommandSpec] = &[
    subcommand("command|list", -2, &["loading", "stale"], &["@slow", "@connection"], &["nondeterministic_output_order"]),
    subcommand("command|info", -2, &["loading", "stale"], &["@slow", "@connection"], &["nondeterministic_output_order"]),
    subcommand("command|docs", -2, &["loading", "stale"], &["@slow", "@connection"], &["nondeterministic_output_order"]),
    subcommand("command|getkeysandflags", -3, &["loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("command|help", 2, &["loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("command|count", 2, &["loading", "stale"], &["@slow", "@connection"], &[]),
    subcommand("command|getkeys", -3, &["loading", "stale"], &["@slow", "@connection"], &[]),
];

const CONFIG_SUBCOMMANDS: &[CommandSpec] = &[
    subcommand("config|get", -3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand(
        "config|rewrite",
        2,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand(
        "config|set",
        -4,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand("config|help", 2, &["loading", "stale"], &["@slow"], &[]),
    subcommand(
        "config|resetstat",
        2,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
];

const ACL_SUBCOMMANDS: &[CommandSpec] = &[
    subcommand("acl|list", 2, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand("acl|genpass", -2, &["noscript", "loading", "stale"], &["@slow"], &[]),
    subcommand("acl|dryrun", -4, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand(
        "acl|deluser",
        -3,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand(
        "acl|save",
        2,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand("acl|log", -2, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand("acl|load", 2, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand("acl|users", 2, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand("acl|getuser", 3, &["admin", "noscript", "loading", "stale"], &["@admin", "@slow", "@dangerous"], &[]),
    subcommand("acl|help", 2, &["loading", "stale"], &["@slow"], &[]),
    subcommand("acl|whoami", 2, &["noscript", "loading", "stale"], &["@slow"], &[]),
    subcommand(
        "acl|setuser",
        -3,
        &["admin", "noscript", "loading", "stale"],
        &["@admin", "@slow", "@dangerous"],
        &["request_policy:all_nodes", "response_policy:all_succeeded"],
    ),
    subcommand("acl|cat", -2, &["noscript", "loading", "stale"], &["@slow"], &[]),
];

/// `COMMAND DOCS` 只覆盖 `get`，与 TS 侧同一范围。
const COMMAND_DOCS: &[(&str, &[DocsValue])] = &[(
    "get",
    &[
        DocsValue::Text("summary"),
        DocsValue::Text("Returns the string value of a key."),
        DocsValue::Text("since"),
        DocsValue::Text("1.0.0"),
        DocsValue::Text("group"),
        DocsValue::Text("string"),
        DocsValue::Text("complexity"),
        DocsValue::Text("O(1)"),
        DocsValue::Text("arguments"),
        DocsValue::List(&[DocsValue::List(&[
            DocsValue::Text("name"),
            DocsValue::Text("key"),
            DocsValue::Text("type"),
            DocsValue::Text("key"),
            DocsValue::Text("display_text"),
            DocsValue::Text("key"),
            DocsValue::Text("key_spec_index"),
            DocsValue::Int(0),
        ])]),
    ],
)];
