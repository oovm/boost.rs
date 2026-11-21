//! RESP2 / RESP3 gateway protocol with transaction queueing.
//!
//! A connection starts on RESP2 and switches to RESP3 when the client sends `HELLO 3`.
//! The negotiated version decides how null replies are spelled: RESP2 uses the null bulk
//! string `$-1\r\n` while RESP3 uses `_\r\n`, and a RESP3 parser rejects the RESP2 form,
//! so the version has to live on the connection rather than be assumed per reply.
//!
//! Key expiry lives in the KV layer: records carry a wall-clock Unix-millisecond
//! deadline, so TTLs are shared by every connection of a listener and survive a
//! listener restart.
//!
//! Command semantics live in `database_execution::cluster::redis`: the string and key
//! space families in `string`, the collection families in `collections`, the transaction
//! session in `session`. This module owns the socket, frame I/O, connection registration,
//! `MULTI` / `EXEC` control frames, `SELECT` / `QUIT` / `HELLO` session state and pub/sub
//! routing, and nothing else.

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use database_execution::cluster::{
    KeyValueExecutor,
    redis::{
        blocking::BlockingRegistry,
        collections,
        reply::{write_bulk, write_error, write_null_array, write_unknown_command},
        session::{self, TransactionSession, lowercase_command, parse_database, parse_integer, watched_keys_modified},
        string::{self, KeyAccessTracker, TrackedExecutor},
    },
};
use database_protocol_redis::codec::{CommandError, Reply};
use database_types::cluster::Namespace;

use super::{
    blocked::{self, BlockingPlan, Family, UnblockMode, Wake},
    pubsub::{ClientSink, PubSubRegistry, SinkRef, encode_ack},
    resp::{FrameError, Request, RequestLimits, decode_request},
    server_face::{self, ClientRecord, ClientRegistry, command_context},
};

/// 阻塞命令等待期间探测对端是否断开的间隔。
pub(crate) const BLOCKED_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Per-connection state carried across RESP2 frames.
///
/// Session-scoped by design: the transaction queue and watch list belong to a single
/// connection and are owned by [`TransactionSession`]. Key expiry is *not* here — it is
/// stored with each KV record and therefore shared by every connection and durable across
/// restarts. Fields `CLIENT LIST` reports live in the shared [`ClientRecord`] instead,
/// because other connections read them.
#[derive(Debug)]
pub(crate) struct ConnectionState {
    /// Protocol version negotiated through `HELLO`. Starts at 2 and becomes 3 once the
    /// client asks for it. Null and map replies are encoded with this version.
    pub(crate) protocol: u8,
    /// `MULTI` queue, watch list and per-command replay flags.
    pub(crate) session: TransactionSession,
    /// 连接在监听器登记表里的投影，`CLIENT LIST` / `CLIENT INFO` / `CLIENT KILL` 读写它。
    pub(crate) record: Arc<ClientRecord>,
}

impl ConnectionState {
    pub(crate) fn new(record: Arc<ClientRecord>) -> Self {
        Self { protocol: 2, session: TransactionSession::new(), record }
    }

    /// Builds the state a queued command is replayed under: blocking commands answer at
    /// once and the negotiated protocol is inherited, so replayed nulls keep the shape the
    /// client handshaked for.
    pub(crate) fn replaying(record: Arc<ClientRecord>, protocol: u8) -> Self {
        Self { protocol, session: TransactionSession::deferred(), record }
    }

    /// 把当前 watch 列表投影到共享连接记录。
    ///
    /// `CLIENT LIST` 的 `watch=` 与 `INFO clients` 的 `watching_clients` /
    /// `total_watched_keys` 读取的都是这份投影；watch 列表只归本连接的会话所有，
    /// 因此每次 `WATCH` / `UNWATCH` / `EXEC` / `DISCARD` / `RESET` 之后都要重新投影。
    pub(crate) fn project_watch(&self) {
        self.record.set_watching(watched_key_count(&self.session));
    }
}

/// 连接当前钉住的逻辑键数。
///
/// `WATCH` 会按值种类把同一个逻辑键钉在六个命名空间上，计数要按「逻辑库 + 键」去重，
/// 与真机 `watch=` 与 `total_watched_keys` 的口径一致；不同 `SELECT` 库上的同名键
/// 属于不同的键，所以库也参与去重。
fn watched_key_count(session: &TransactionSession) -> usize {
    if session.watched().is_empty() {
        return 0;
    }
    let mut logical: Vec<(String, &[u8])> = Vec::new();
    for entry in session.watched() {
        let mut base = entry.namespace.0.as_str();
        for kind in collections::KeyKind::ALL {
            let suffix = format!(":{}", kind.name());
            if let Some(stripped) = base.strip_suffix(&suffix) {
                base = stripped;
                break;
            }
        }
        let slot = (base.to_owned(), entry.key.as_slice());
        if !logical.contains(&slot) {
            logical.push(slot);
        }
    }
    logical.len()
}

/// Serves one bounded RESP2 connection with a 30-second I/O timeout.
/// Only PING, ECHO and QUIT are supported. Database commands require a binder.
pub fn serve_connection(stream: TcpStream, limits: RequestLimits) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let reader = stream.try_clone()?;
    let (remote, local) = socket_pair(&stream);
    let sink = Arc::new(ClientSink::new(stream, None));
    serve_io(
        reader,
        sink,
        remote,
        local,
        limits,
        None,
        Arc::new(KeyAccessTracker::new()),
        Arc::new(BlockingRegistry::new()),
        Arc::new(PubSubRegistry::new()),
        None,
    )
}

/// Serves a node-owned connection with cooperative cancellation of all socket I/O.
pub fn serve_cancellable(stream: TcpStream, limits: RequestLimits, stopping: Arc<AtomicBool>) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let (remote, local) = socket_pair(&stream);
    let reader = CancellableSocket { stream: stream.try_clone()?, stopping: Arc::clone(&stopping) };
    let sink = Arc::new(ClientSink::new_polling(stream, Some(stopping)));
    serve_io(
        reader,
        sink,
        remote,
        local,
        limits,
        None,
        Arc::new(KeyAccessTracker::new()),
        Arc::new(BlockingRegistry::new()),
        Arc::new(PubSubRegistry::new()),
        None,
    )
}

/// Serves a node-owned RESP2 connection using the YYDS key/value execution contract.
pub fn serve_cancellable_with_executor(
    stream: TcpStream,
    limits: RequestLimits,
    stopping: Arc<AtomicBool>,
    executor: Arc<dyn KeyValueExecutor>,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let (remote, local) = socket_pair(&stream);
    let reader = CancellableSocket { stream: stream.try_clone()?, stopping: Arc::clone(&stopping) };
    let sink = Arc::new(ClientSink::new_polling(stream, Some(stopping)));
    serve_io(
        reader,
        sink,
        remote,
        local,
        limits,
        Some(executor),
        Arc::new(KeyAccessTracker::new()),
        Arc::new(BlockingRegistry::new()),
        Arc::new(PubSubRegistry::new()),
        None,
    )
}

/// Serves a node-owned RESP2 connection while sharing one access tracker with its listener.
///
/// The tracker holds `OBJECT IDLETIME` state, which is per-object in Redis and therefore
/// has to be shared by every connection of the same listener rather than recreated per
/// socket. `registry` carries the blocking wait table and `pubsub` the channel table, both
/// shared for the same reason: waiters and subscribers on one connection are reached from
/// another. `clients` is the connection table behind `CLIENT LIST` / `CLIENT KILL`; when it
/// is `None` the connection gets a private table, which keeps a lone probe connection's
/// `CLIENT ID` stable at `1` like the TypeScript fallback.
pub fn serve_cancellable_with_executor_and_tracker(
    stream: TcpStream,
    limits: RequestLimits,
    stopping: Arc<AtomicBool>,
    executor: Arc<dyn KeyValueExecutor>,
    tracker: Arc<KeyAccessTracker>,
    registry: Arc<BlockingRegistry>,
    pubsub: Arc<PubSubRegistry>,
    clients: Option<Arc<ClientRegistry>>,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let (remote, local) = socket_pair(&stream);
    let reader = CancellableSocket { stream: stream.try_clone()?, stopping: Arc::clone(&stopping) };
    let sink = Arc::new(ClientSink::new_polling(stream, Some(stopping)));
    serve_io(reader, sink, remote, local, limits, Some(executor), tracker, registry, pubsub, clients)
}

/// `host:port` pair of a socket, defaulting to an empty string when unavailable.
fn socket_pair(stream: &TcpStream) -> (String, String) {
    let remote = stream.peer_addr().map(|address| address.to_string()).unwrap_or_default();
    let local = stream.local_addr().map(|address| address.to_string()).unwrap_or_default();
    (remote, local)
}

struct CancellableSocket {
    stream: TcpStream,
    stopping: Arc<AtomicBool>,
}

impl CancellableSocket {
    fn attempt<T>(&mut self, mut operation: impl FnMut(&mut TcpStream) -> io::Result<T>) -> io::Result<T> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "node service stopping"));
            }
            match operation(&mut self.stream) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "socket I/O timeout"));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl Read for CancellableSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.attempt(|stream| stream.read(bytes))
    }
}

impl Write for CancellableSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.attempt(|stream| stream.write(bytes))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.attempt(Write::flush)
    }
}

fn serve_io(
    mut reader: impl Read,
    sink: Arc<ClientSink>,
    remote: String,
    local: String,
    limits: RequestLimits,
    executor: Option<Arc<dyn KeyValueExecutor>>,
    tracker: Arc<KeyAccessTracker>,
    registry: Arc<BlockingRegistry>,
    pubsub: Arc<PubSubRegistry>,
    clients: Option<Arc<ClientRegistry>>,
) -> io::Result<()> {
    let clients = clients.unwrap_or_else(|| Arc::new(ClientRegistry::standalone()));
    // 订阅登记先拿到内部 id，连接登记再带上它：`CLIENT LIST` 的 sub=/psub= 要用这个 id
    // 去订阅表取计数。
    let client = pubsub.attach(Arc::clone(&sink));
    let record = clients.attach(Arc::clone(&sink), client, remote, local);
    let _subscription_guard = SubscriptionGuard { registry: Arc::clone(&pubsub), client };
    let _client_guard = ClientGuard { registry: Arc::clone(&clients), id: record.id };
    let mut writer = SinkRef(Arc::clone(&sink));
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut namespace = Namespace("redis:0".into());
    let mut state = ConnectionState::new(Arc::clone(&record));
    // 所有普通命令走包装执行器以刷新访问计时，`OBJECT` 与 `WATCH` 需要不记时地查键，
    // 因此另保留一个未包装的执行器视图。
    let tracked = executor.as_deref().map(|inner| TrackedExecutor::new(inner, tracker.as_ref()));
    let tracked_view: Option<&dyn KeyValueExecutor> = tracked.as_ref().map(|wrapper| wrapper as &dyn KeyValueExecutor);
    loop {
        loop {
            match decode_request(&pending, limits) {
                Ok(Some(request)) => {
                    let consumed = request.consumed;
                    let result = respond(
                        &mut writer as &mut dyn Write,
                        &request,
                        tracked_view,
                        executor.as_deref(),
                        tracker.as_ref(),
                        registry.as_ref(),
                        pubsub.as_ref(),
                        clients.as_ref(),
                        client,
                        &sink,
                        &mut namespace,
                        &mut state,
                    );
                    if !result? {
                        return Ok(());
                    }
                    // 一条命令可能写入了阻塞命令等待的数据，也可能叫停了别的连接：唤醒所有
                    // 等待者重新判定队首并重试取数。
                    clients.blocking().wake();
                    pending.drain(..consumed);
                }
                Ok(None) => break,
                // `*0\r\n` 是非法多批量长度，真机回复协议错误并关闭连接；静默丢弃
                // 会让等待回复的客户端一直挂起。
                Err(FrameError::EmptyCommand) => {
                    writer.write_all(b"-ERR Protocol error or request limit exceeded\r\n")?;
                    return Ok(());
                }
                Err(_) => {
                    writer.write_all(b"-ERR Protocol error or request limit exceeded\r\n")?;
                    return Ok(());
                }
            }
        }
        let available = limits.max_frame_bytes.saturating_sub(pending.len()).min(chunk.len());
        if available == 0 {
            writer.write_all(b"-ERR Protocol error or request limit exceeded\r\n")?;
            return Ok(());
        }
        let received = match reader.read(&mut chunk[..available]) {
            Ok(received) => received,
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => return Ok(()),
            Err(error) => return Err(error),
        };
        if received == 0 {
            if !pending.is_empty() {
                writer.write_all(b"-ERR Truncated request\r\n")?
            }
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..received]);
        clients.add_input(received);
    }
}

/// Removes the connection's subscriptions when its read loop ends.
struct SubscriptionGuard {
    registry: Arc<PubSubRegistry>,
    client: u64,
}

/// 连接结束时从 `CLIENT LIST` 的登记表里移除自己。
struct ClientGuard {
    registry: Arc<ClientRegistry>,
    id: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.registry.detach(self.id);
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.registry.detach(self.client);
    }
}

/// `WATCH key [key ...]` — pins each logical key's current revision as the version `EXEC` validates.
///
/// 一个逻辑键在任一时刻只活在一种值种类里，但写命令落在各自的种类命名空间上
/// （`SADD` 写 `:set`、`LPUSH` 写 `:list`、`HSET` 写 `:hash`），而 `EXPIRE` 之类的
/// TTL 写会重写该种类自己的记录。只钉字符串命名空间会让这些写对 `EXEC` 不可见，因此
/// 这里把六种值种类的当前修订号一起钉住：键从缺席变成某种值、或某种值被改写/删除，
/// 都会让 `EXEC` 放弃事务。
///
/// A repeated `WATCH` of an already-pinned key keeps the original revisions, and a missing
/// key is pinned as absent in every kind, so a later insert also aborts the transaction.
/// Watching inside `MULTI` is rejected without touching the existing watch list, like
/// upstream Redis. 真正的空操作写（重复 `SADD` 已存在成员）不提交记录、不推进修订号，
/// 因此不会让事务失效，与真机一致。
fn handle_watch(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
    state: &mut ConnectionState,
) -> io::Result<()> {
    let count = request.arguments.len();
    if count < 2 {
        stream.write_all(b"-ERR wrong number of arguments for 'watch' command\r\n")?;
        return Ok(());
    }
    if state.session.in_multi() {
        stream.write_all(b"-ERR WATCH inside MULTI is not allowed\r\n")?;
        return Ok(());
    }
    let Some(executor) = executor
    else {
        stream.write_all(b"-ERR unsupported command\r\n")?;
        return Ok(());
    };
    for key in &request.arguments[1..] {
        if state.session.is_watching(namespace, key) {
            continue;
        }
        let mut pinned = Vec::with_capacity(collections::KeyKind::ALL.len());
        for kind in collections::KeyKind::ALL {
            let kind_namespace = kind.namespace(namespace);
            let revision = match session::key_revision(executor, &kind_namespace, key) {
                Ok(revision) => revision,
                Err(message) => {
                    // 已经钉住的键照样属于这条连接，先把投影对齐再回错误。
                    state.project_watch();
                    write_error(stream, &message)?;
                    return Ok(());
                }
            };
            pinned.push((kind_namespace, revision));
        }
        for (kind_namespace, revision) in pinned {
            state.session.watch(&kind_namespace, key, revision);
        }
    }
    state.project_watch();
    stream.write_all(b"+OK\r\n")
}

/// `UNWATCH` — forgets every pinned key without touching the transaction queue.
fn handle_unwatch(stream: &mut dyn Write, count: usize, state: &mut ConnectionState) -> io::Result<()> {
    if count != 1 {
        stream.write_all(b"-ERR wrong number of arguments for 'unwatch' command\r\n")?;
        return Ok(());
    }
    state.session.unwatch();
    state.project_watch();
    stream.write_all(b"+OK\r\n")
}

fn respond(
    stream: &mut dyn Write,
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: &KeyAccessTracker,
    registry: &BlockingRegistry,
    pubsub: &PubSubRegistry,
    clients: &ClientRegistry,
    client: u64,
    sink: &Arc<ClientSink>,
    namespace: &mut Namespace,
    state: &mut ConnectionState,
) -> io::Result<bool> {
    let command = request.arguments[0];
    let count = request.arguments.len();
    // `CLIENT LIST` 的 cmd= / tot-cmds= 来自这里：每条命令都推进一次活动时间，
    // 容器命令记 `parent|sub` 形式。
    state.record.touch(command_context(command, &request.arguments));
    clients.bump_command();

    // ---- RESP2 subscriber mode: only the pub/sub verbs, PING, QUIT and RESET may run.
    // RESP3 lifted this restriction and allows any command while subscribed.
    if state.protocol == 2 && pubsub.is_subscribed(client) && !subscriber_command_allowed(command) {
        let message = format!(
            "-ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context\r\n",
            lowercase_command(command)
        );
        stream.write_all(message.as_bytes())?;
        return Ok(true);
    }

    // ---- Transaction control: MULTI / EXEC / DISCARD. These are always handled directly
    // and never queued (Redis would reject a second MULTI inside a MULTI block anyway).

    if command.eq_ignore_ascii_case(b"MULTI") {
        if count != 1 {
            stream.write_all(b"-ERR wrong number of arguments for 'multi' command\r\n")?;
            return Ok(true);
        }
        if state.session.in_multi() {
            // 真机把嵌套 `MULTI` 当作控制面拒绝：报错但不毒化事务，队列中的命令仍会被
            // `EXEC` 执行。只有排队命令自身的语法/arity 失败才置中毒标记。
            stream.write_all(b"-ERR MULTI calls can not be nested\r\n")?;
            return Ok(true);
        }
        state.session.begin_multi();
        state.record.set_multi(Some(0));
        stream.write_all(b"+OK\r\n")?;
        return Ok(true);
    }

    if command.eq_ignore_ascii_case(b"EXEC") {
        if count != 1 {
            stream.write_all(b"-ERR wrong number of arguments for 'exec' command\r\n")?;
            return Ok(true);
        }
        if !state.session.in_multi() {
            stream.write_all(b"-ERR EXEC without MULTI\r\n")?;
            return Ok(true);
        }
        // A rejected queued command poisons the transaction: EXEC answers EXECABORT and
        // drops the whole queue without running anything.
        if state.session.multi_error() {
            state.session.discard();
            state.record.set_multi(None);
            state.project_watch();
            stream.write_all(b"-EXECABORT Transaction discarded because of previous errors.\r\n")?;
            return Ok(true);
        }
        // Optimistic locking: any write to a watched key since WATCH abandons the whole
        // transaction before a single queued command runs. The reply is the nil array.
        if !state.session.watched().is_empty() {
            // 版本比对只读取修订号，不刷新访问计时，否则 `EXEC` 前的探测会把空闲时间清零。
            let modified = match probe {
                Some(probe) => watched_keys_modified(probe, state.session.watched()),
                None => Ok(true),
            };
            match modified {
                Ok(false) => {}
                Ok(true) => {
                    write_null_array(stream, state.protocol)?;
                    state.session.discard();
                    state.record.set_multi(None);
                    state.project_watch();
                    return Ok(true);
                }
                Err(message) => {
                    write_error(stream, &message)?;
                    state.session.discard();
                    state.record.set_multi(None);
                    state.project_watch();
                    return Ok(true);
                }
            }
        }
        execute_transaction(stream, executor, probe, tracker, registry, pubsub, clients, client, sink, namespace, state)?;
        return Ok(true);
    }

    if command.eq_ignore_ascii_case(b"DISCARD") {
        if count != 1 {
            stream.write_all(b"-ERR wrong number of arguments for 'discard' command\r\n")?;
            return Ok(true);
        }
        if !state.session.in_multi() {
            stream.write_all(b"-ERR DISCARD without MULTI\r\n")?;
            return Ok(true);
        }
        state.session.discard();
        state.record.set_multi(None);
        state.project_watch();
        stream.write_all(b"+OK\r\n")?;
        return Ok(true);
    }

    // ---- Optimistic locking control: WATCH / UNWATCH. `WATCH` must run before `MULTI`,
    // while `UNWATCH` inside a `MULTI` is queued like any other command, as upstream does.
    if command.eq_ignore_ascii_case(b"WATCH") {
        handle_watch(stream, request, probe, namespace, state)?;
        return Ok(true);
    }

    if command.eq_ignore_ascii_case(b"UNWATCH") {
        if state.session.in_multi() {
            if count != 1 {
                stream.write_all(b"-ERR wrong number of arguments for 'unwatch' command\r\n")?;
                return Ok(true);
            }
            state.session.queue(&request.arguments);
            stream.write_all(b"+QUEUED\r\n")?;
            return Ok(true);
        }
        handle_unwatch(stream, count, state)?;
        return Ok(true);
    }

    // ---- Inside a MULTI block: buffer every other command and return QUEUED.
    // Syntax / arity validation happens here so clients learn about bad commands at queue
    // time instead of at EXEC time. The actual runtime behaviour (wrong-type, NX failure,
    // etc.) is deferred to EXEC per Redis semantics.
    if state.session.in_multi() {
        if let Err(error) = session::validate_command_for_queue(command, count, &request.arguments, executor.is_some(), namespace) {
            stream.write_all(&database_protocol_redis::codec::encode_reply(&error.to_reply(), state.protocol))?;
            // 同 TypeScript 侧：规划失败立刻毒化事务，EXEC 整队丢弃。
            state.session.poison();
            return Ok(true);
        }
        let queued = state.session.queue(&request.arguments);
        state.record.set_multi(Some(queued));
        stream.write_all(b"+QUEUED\r\n")?;
        return Ok(true);
    }

    // ---- Below is the normal, command-by-command execution path.

    if command.eq_ignore_ascii_case(b"AUTH") {
        // 本地网关没有口令模型。真机在未配置密码时对 AUTH 回复的正是这条文案，
        // 客户端据此区分「服务端未设密码」与「命令不存在」。
        if count == 2 || count == 3 {
            stream.write_all(b"-ERR Client sent AUTH, but no password is set. Did you mean AUTH <username> <password>?\r\n")?;
        }
        else {
            stream.write_all(b"-ERR wrong number of arguments for 'auth' command\r\n")?;
        }
        return Ok(true);
    }

    if command.eq_ignore_ascii_case(b"HELLO") {
        // Modern clients (redis-py 8, ioredis, redis-cli) send `HELLO 3` on connect. The
        // answered version becomes the connection's protocol: subsequent null replies use
        // `_\r\n` under RESP3 and the null bulk string under RESP2. `HELLO` without an
        // argument reports the version already in force, like upstream Redis.
        let protocol = match request.arguments.get(1) {
            None => state.protocol,
            Some(raw) => match parse_integer(raw) {
                Some(2) => 2,
                Some(3) => 3,
                Some(_) => {
                    stream.write_all(b"-NOPROTO unsupported protocol version\r\n")?;
                    return Ok(true);
                }
                None => {
                    stream.write_all(b"-ERR Protocol version is not an integer or out of range\r\n")?;
                    return Ok(true);
                }
            },
        };
        let mut index = 2;
        while index < count {
            let option = request.arguments[index];
            if option.eq_ignore_ascii_case(b"AUTH") && index + 2 < count {
                // The local gateway has no credential model, so HELLO AUTH is accepted
                // for the same reason `yyds` accepts any MySQL user.
                index += 3;
            }
            else if option.eq_ignore_ascii_case(b"SETNAME") && index + 1 < count {
                index += 2;
            }
            else {
                stream.write_all(b"-ERR Syntax error in HELLO option\r\n")?;
                return Ok(true);
            }
        }
        write_hello(stream, protocol)?;
        state.protocol = protocol;
        state.record.set_protocol(protocol);
        sink.set_protocol(protocol);
    }
    else if command.eq_ignore_ascii_case(b"PING") {
        match count {
            1 => {
                // In RESP2 subscriber mode PING answers the two-element `pong` array
                // instead of the simple string; RESP3 keeps the ordinary reply.
                if state.protocol == 2 && pubsub.is_subscribed(client) {
                    stream.write_all(b"*2\r\n$4\r\npong\r\n$0\r\n\r\n")?;
                }
                else {
                    stream.write_all(b"+PONG\r\n")?;
                }
            }
            2 => write_bulk(stream, request.arguments[1])?,
            _ => stream.write_all(b"-ERR wrong number of arguments for 'ping' command\r\n")?,
        }
    }
    else if command.eq_ignore_ascii_case(b"RESET") {
        if count != 1 {
            stream.write_all(b"-ERR wrong number of arguments for 'reset' command\r\n")?;
            return Ok(true);
        }
        // RESET drops subscriptions, leaves MULTI, rewrites the protocol back to RESP2
        // and returns to database 0, which is the state a fresh connection starts in.
        for (name, _) in pubsub.unsubscribe(client, &[], false) {
            let _ = name;
        }
        for (name, _) in pubsub.unsubscribe(client, &[], true) {
            let _ = name;
        }
        state.session.discard();
        state.protocol = 2;
        state.record.set_protocol(2);
        state.record.set_multi(None);
        state.record.set_database(0);
        state.record.set_name(None);
        state.project_watch();
        sink.set_protocol(2);
        *namespace = Namespace("redis:0".into());
        stream.write_all(b"+RESET\r\n")?;
    }
    else if command.eq_ignore_ascii_case(b"SUBSCRIBE") {
        handle_subscribe(stream, pubsub, client, state.protocol, &request.arguments, false)?;
    }
    else if command.eq_ignore_ascii_case(b"UNSUBSCRIBE") {
        handle_unsubscribe(stream, pubsub, client, state.protocol, &request.arguments, false)?;
    }
    else if command.eq_ignore_ascii_case(b"PSUBSCRIBE") {
        handle_subscribe(stream, pubsub, client, state.protocol, &request.arguments, true)?;
    }
    else if command.eq_ignore_ascii_case(b"PUNSUBSCRIBE") {
        handle_unsubscribe(stream, pubsub, client, state.protocol, &request.arguments, true)?;
    }
    else if command.eq_ignore_ascii_case(b"PUBLISH") {
        handle_publish(stream, pubsub, &request.arguments)?;
    }
    else if command.eq_ignore_ascii_case(b"PUBSUB") {
        handle_pubsub(stream, pubsub, &request.arguments)?;
    }
    else if command.eq_ignore_ascii_case(b"ECHO") {
        if count == 2 {
            write_bulk(stream, request.arguments[1])?
        }
        else {
            stream.write_all(b"-ERR wrong number of arguments for 'echo' command\r\n")?
        }
    }
    else if command.eq_ignore_ascii_case(b"QUIT") {
        // 真机忽略 QUIT 的多余参数，一律回 OK 并断连。
        stream.write_all(b"+OK\r\n")?;
        return Ok(false);
    }
    else if command.eq_ignore_ascii_case(b"CLIENT") {
        if !server_face::handle_client(stream, request, state, clients, &pubsub.state())? {
            return Ok(false);
        }
    }
    else if command.eq_ignore_ascii_case(b"SELECT") && executor.is_some() {
        if count != 2 {
            stream.write_all(b"-ERR wrong number of arguments for 'select' command\r\n")?;
        }
        else if let Some(database) = parse_database(request.arguments[1]) {
            *namespace = string::database_namespace(namespace, u32::from(database));
            state.record.set_database(database);
            stream.write_all(b"+OK\r\n")?;
        }
        else {
            stream.write_all(b"-ERR DB index is out of range\r\n")?;
        }
    }
    else if command.eq_ignore_ascii_case(b"INFO") {
        server_face::handle_info(stream, request, clients, &pubsub.state(), executor)?;
    }
    else if command.eq_ignore_ascii_case(b"CONFIG") {
        server_face::handle_config(stream, request, clients)?;
    }
    else if command.eq_ignore_ascii_case(b"ACL") {
        server_face::handle_acl(stream, request, state.protocol)?;
    }
    else if command.eq_ignore_ascii_case(b"COMMAND") {
        server_face::handle_command(stream, request, state.protocol)?;
    }
    else {
        // 字符串与键空间族产出类型化 Reply / CommandError，字节统一由 protocol codec 编码；
        // `Ok(None)` 表示非本族命令，继续走容器族分派与未知命令兜底。
        match string::handle_string(request, executor, probe, Some(tracker), namespace, state.protocol) {
            Ok(Some(reply)) => stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?,
            Err(error) => stream.write_all(&database_protocol_redis::codec::encode_reply(&error.to_reply(), state.protocol))?,
            Ok(None) => {
                if collections::is_hyperloglog_command(command) {
                    let reply = collections::handle_hyperloglog(request, executor, namespace).unwrap_or_else(|error| error.to_reply());
                    stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?
                }
                else if collections::is_hash_command(command) {
                    let reply = collections::handle_hash(request, executor, namespace, state.protocol).unwrap_or_else(|error| error.to_reply());
                    stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?
                }
                else if collections::is_list_command(command) {
                    let reply = collection_reply(
                        Family::List,
                        command,
                        request,
                        executor,
                        namespace,
                        state.protocol,
                        state.session.defer_blocking(),
                        registry,
                        clients,
                        &state.record,
                    );
                    stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?
                }
                else if collections::is_set_command(command) {
                    let reply = collections::handle_set(request, executor, namespace, state.protocol).unwrap_or_else(|error| error.to_reply());
                    stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?
                }
                else if collections::is_zset_command(command) {
                    let reply = collection_reply(
                        Family::Zset,
                        command,
                        request,
                        executor,
                        namespace,
                        state.protocol,
                        state.session.defer_blocking(),
                        registry,
                        clients,
                        &state.record,
                    );
                    stream.write_all(&database_protocol_redis::codec::encode_reply(&reply, state.protocol))?
                }
                else {
                    write_unknown_command(stream, &request.arguments)?;
                }
            }
        }
    }
    Ok(true)
}

/// Runs every queued command under a fresh per-command `ConnectionState` so that MULTI
/// control frames cannot nest into the transaction. Each command's RESP output is captured
/// into a buffer and then emitted wrapped in a Redis multi-bulk array header.
///
/// This is a per-command replay over [`KeyValueExecutor::execute`], not an atomic commit
/// boundary: each queued command is applied on its own, a mid-transaction failure does not
/// roll back the commands already applied, and the executor's durable `commit_batch` path is
/// deliberately not used. Upstream Redis queues and executes commands sequentially the same
/// way, so no all-or-nothing durability is promised on this surface.
fn execute_transaction(
    stream: &mut dyn Write,
    executor: Option<&dyn KeyValueExecutor>,
    probe: Option<&dyn KeyValueExecutor>,
    tracker: &KeyAccessTracker,
    registry: &BlockingRegistry,
    pubsub: &PubSubRegistry,
    clients: &ClientRegistry,
    client: u64,
    sink: &Arc<ClientSink>,
    namespace: &mut Namespace,
    state: &mut ConnectionState,
) -> io::Result<()> {
    let queued = state.session.finish_exec();
    let count = queued.len();
    let protocol = state.protocol;
    // `EXEC` always releases the watched keys, whether the queue was non-empty or not.
    state.record.set_multi(None);
    state.project_watch();

    write!(stream, "*{count}\r\n")?;

    for queued in queued {
        let owned = queued.request;
        let inner_request = owned.as_request();
        let mut buf = Vec::<u8>::new();
        let mut inner_namespace = namespace.clone();
        // Give each queued command a fresh state so nested MULTI/EXEC/DISCARD semantics
        // do not pollute the parent transaction. The negotiated protocol is inherited, so
        // replayed nulls keep the shape the client handshaked for. Key expiry lives in the
        // KV records, so queued commands see and write it through the executor like any
        // other state. Blocking commands never wait inside `EXEC`; they take the queued
        // command's immediate answer, which is the same as upstream.
        let mut inner_state = ConnectionState::replaying(Arc::clone(&state.record), protocol);
        let _ = respond(
            &mut buf as &mut dyn Write,
            &inner_request,
            executor,
            probe,
            tracker,
            registry,
            pubsub,
            clients,
            client,
            sink,
            &mut inner_namespace,
            &mut inner_state,
        );
        stream.write_all(&buf)?;
    }
    Ok(())
}

/// Commands Redis still honours while a RESP2 connection is in subscriber mode.
pub(crate) fn subscriber_command_allowed(command: &[u8]) -> bool {
    command.eq_ignore_ascii_case(b"SUBSCRIBE")
        || command.eq_ignore_ascii_case(b"UNSUBSCRIBE")
        || command.eq_ignore_ascii_case(b"PSUBSCRIBE")
        || command.eq_ignore_ascii_case(b"PUNSUBSCRIBE")
        || command.eq_ignore_ascii_case(b"PING")
        || command.eq_ignore_ascii_case(b"QUIT")
        || command.eq_ignore_ascii_case(b"RESET")
}

/// 集合族命令的一次分派：阻塞命令停在连接线程里等数据，其余命令按原语义立刻执行。
///
/// `defer_blocking` 为真（`EXEC` 复放）时阻塞命令按真机的非阻塞语义回一次结果，不会让
/// 持有事务的连接睡在键上。
#[allow(clippy::too_many_arguments)]
fn collection_reply(
    family: Family,
    command: &[u8],
    request: &Request<'_>,
    executor: Option<&dyn KeyValueExecutor>,
    namespace: &Namespace,
    protocol: u8,
    defer_blocking: bool,
    registry: &BlockingRegistry,
    clients: &ClientRegistry,
    record: &Arc<ClientRecord>,
) -> Reply {
    let Some(executor) = executor
    else {
        return CommandError::Failed("unsupported command".to_owned()).to_reply();
    };
    match blocked::plan(command, &request.arguments).filter(|plan| plan.family == family) {
        Some(plan) if !defer_blocking => {
            serve_blocking(&plan, command, &request.arguments, executor, namespace, protocol, registry, clients, record)
        }
        _ => match family {
            Family::List => collections::handle_list(request, Some(executor), namespace, protocol, registry, !defer_blocking),
            Family::Zset => collections::handle_zset(request, Some(executor), namespace, protocol),
        }
        .unwrap_or_else(|error| error.to_reply()),
    }
}

/// 执行一条阻塞命令：先按非阻塞语义取一次，取不到就登记等待，直到数据到达、超时或
/// `CLIENT UNBLOCK`。
///
/// 取数语义仍由集合命令处理器负责，这里只决定「什么时候、用哪些键重试」：只有排在某个键
/// 队首的连接才有权从该键取数，因此同一键上的等待者按登记顺序被服务。数据只能由别的命令
/// 写入，所以等待者由命令完成时的广播唤醒，而不是靠轮询。
#[allow(clippy::too_many_arguments)]
fn serve_blocking(
    plan: &BlockingPlan,
    command: &[u8],
    arguments: &[&[u8]],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
    registry: &BlockingRegistry,
    clients: &ClientRegistry,
    record: &Arc<ClientRecord>,
) -> Reply {
    let owned: Vec<Vec<u8>> = arguments.iter().map(|argument| argument.to_vec()).collect();
    // 第一次尝试同时完成 arity / 超时 / 类型校验，因此错误在这里就回给客户端。
    match attempt(plan.family, &owned, executor, namespace, protocol, registry) {
        Ok(reply) if !empty_reply(&reply) => return reply,
        Ok(_) => {}
        Err(error) => return error.to_reply(),
    }
    let deadline = blocked::deadline(&plan.timeout);
    let keys: Vec<&[u8]> = plan.key_indices.iter().map(|index| arguments[*index]).collect();
    let table = clients.blocking();
    let slots = table.register(record.id, namespace, &keys);
    record.set_blocked(true);
    // 阻塞等待期间读循环停在这里，没人再读 socket；只有主动探测才能发现客户端直接断开
    // 或 `CLIENT KILL`。探测只对非阻塞 socket 的写侧可用，其它连接保持原先的等待方式。
    let poll = record.blocked_poll_interval();
    let mut observed = table.generation();
    let reply = loop {
        if record.is_closed() || record.peer_closed() {
            break Reply::NullArray;
        }
        let heads: Vec<usize> = plan
            .key_indices
            .iter()
            .copied()
            .filter(|index| table.is_head(record.id, &(namespace.clone(), arguments[*index].to_vec())))
            .collect();
        if !heads.is_empty() {
            let rebuilt = blocked::head_arguments(command, arguments, plan, &heads);
            match attempt(plan.family, &rebuilt, executor, namespace, protocol, registry) {
                Ok(reply) if !empty_reply(&reply) => break reply,
                Ok(_) => {}
                Err(error) => break error.to_reply(),
            }
        }
        // 一次最多睡一个探测间隔，醒来先重探 socket，再看整体超时是否已到。
        let now = Instant::now();
        let slice = match (deadline, poll) {
            (Some(deadline), Some(interval)) => Some(deadline.min(now + interval)),
            (Some(deadline), None) => Some(deadline),
            (None, Some(interval)) => Some(now + interval),
            (None, None) => None,
        };
        match table.wait(record.id, slice, observed) {
            Wake::TimedOut if deadline.is_some_and(|deadline| Instant::now() >= deadline) => break Reply::NullArray,
            Wake::TimedOut => {}
            Wake::Unblocked(UnblockMode::Timeout) => break Reply::NullArray,
            Wake::Unblocked(UnblockMode::Error) => {
                break Reply::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_owned());
            }
            Wake::Signaled(generation) => observed = generation,
        }
    };
    table.release(record.id, &slots);
    record.set_blocked(false);
    reply
}

/// 按非阻塞语义执行一次阻塞命令，返回处理器给出的回复。
fn attempt(
    family: Family,
    arguments: &[Vec<u8>],
    executor: &dyn KeyValueExecutor,
    namespace: &Namespace,
    protocol: u8,
    registry: &BlockingRegistry,
) -> Result<Reply, CommandError> {
    let borrowed: Vec<&[u8]> = arguments.iter().map(Vec::as_slice).collect();
    let request = Request { arguments: borrowed, consumed: 0 };
    match family {
        Family::List => collections::handle_list(&request, Some(executor), namespace, protocol, registry, false),
        Family::Zset => collections::handle_zset(&request, Some(executor), namespace, protocol),
    }
}

/// 阻塞命令「这一次没取到数据」的回复形状：null bulk 或 null array。
fn empty_reply(reply: &Reply) -> bool {
    matches!(reply, Reply::Null | Reply::NullArray)
}

/// One `SUBSCRIBE` or `PSUBSCRIBE`: registers each name and answers one ack frame per name.
fn handle_subscribe(
    stream: &mut dyn Write,
    pubsub: &PubSubRegistry,
    client: u64,
    protocol: u8,
    arguments: &[&[u8]],
    pattern: bool,
) -> io::Result<()> {
    if arguments.len() < 2 {
        let kind = if pattern { "psubscribe" } else { "subscribe" };
        stream.write_all(format!("-ERR wrong number of arguments for '{kind}' command\r\n").as_bytes())?;
        return Ok(());
    }
    let kind: &[u8] = if pattern { b"psubscribe" } else { b"subscribe" };
    for name in &arguments[1..] {
        let remaining = pubsub.subscribe(client, name, pattern);
        stream.write_all(&encode_ack(protocol, kind, Some(name), remaining))?;
    }
    Ok(())
}

/// One `UNSUBSCRIBE` or `PUNSUBSCRIBE`: removes each name, or everything when none given.
fn handle_unsubscribe(
    stream: &mut dyn Write,
    pubsub: &PubSubRegistry,
    client: u64,
    protocol: u8,
    arguments: &[&[u8]],
    pattern: bool,
) -> io::Result<()> {
    let kind: &[u8] = if pattern { b"punsubscribe" } else { b"unsubscribe" };
    let names: Vec<&[u8]> = arguments[1..].to_vec();
    for (name, remaining) in pubsub.unsubscribe(client, &names, pattern) {
        stream.write_all(&encode_ack(protocol, kind, name.as_deref(), remaining))?;
    }
    Ok(())
}

/// `PUBLISH`: pushes to every matching subscription and answers the receiver count.
fn handle_publish(stream: &mut dyn Write, pubsub: &PubSubRegistry, arguments: &[&[u8]]) -> io::Result<()> {
    if arguments.len() != 3 {
        stream.write_all(b"-ERR wrong number of arguments for 'publish' command\r\n")?;
        return Ok(());
    }
    let delivered = pubsub.publish(arguments[1], arguments[2]);
    stream.write_all(format!(":{delivered}\r\n").as_bytes())
}

/// `PUBSUB CHANNELS | NUMSUB | NUMPAT`.
fn handle_pubsub(stream: &mut dyn Write, pubsub: &PubSubRegistry, arguments: &[&[u8]]) -> io::Result<()> {
    if arguments.len() < 2 {
        stream.write_all(b"-ERR wrong number of arguments for 'pubsub' command\r\n")?;
        return Ok(());
    }
    let subcommand = arguments[1];
    if subcommand.eq_ignore_ascii_case(b"CHANNELS") {
        if arguments.len() > 3 {
            return write_pubsub_subcommand_error(stream, subcommand);
        }
        let channels = pubsub.channels(arguments.get(2).copied());
        stream.write_all(format!("*{}\r\n", channels.len()).as_bytes())?;
        for channel in channels {
            write_bulk(stream, &channel)?;
        }
        return Ok(());
    }
    if subcommand.eq_ignore_ascii_case(b"NUMSUB") {
        let channels: Vec<&[u8]> = arguments[2..].to_vec();
        let pairs = pubsub.numsub(&channels);
        stream.write_all(format!("*{}\r\n", pairs.len() * 2).as_bytes())?;
        for (channel, count) in pairs {
            write_bulk(stream, &channel)?;
            stream.write_all(format!(":{count}\r\n").as_bytes())?;
        }
        return Ok(());
    }
    if subcommand.eq_ignore_ascii_case(b"NUMPAT") {
        if arguments.len() != 2 {
            return write_pubsub_subcommand_error(stream, subcommand);
        }
        stream.write_all(format!(":{}\r\n", pubsub.numpat()).as_bytes())?;
        return Ok(());
    }
    let name = String::from_utf8_lossy(subcommand);
    stream.write_all(format!("-ERR unknown subcommand '{name}'. Try PUBSUB HELP.\r\n").as_bytes())
}

/// Error Redis returns for a known `PUBSUB` subcommand with the wrong argument count.
fn write_pubsub_subcommand_error(stream: &mut dyn Write, subcommand: &[u8]) -> io::Result<()> {
    let name = String::from_utf8_lossy(subcommand);
    stream.write_all(format!("-ERR unknown subcommand or wrong number of arguments for '{name}'. Try PUBSUB HELP.\r\n").as_bytes())
}

/// Writes the `HELLO` reply in the protocol version the client asked for:
/// a RESP3 map for `HELLO 3`, or the same key/value pairs as a flat RESP2 array.
fn write_hello(stream: &mut dyn Write, protocol: u8) -> io::Result<()> {
    stream.write_all(if protocol == 3 { b"%7\r\n" as &[u8] } else { b"*14\r\n" })?;
    write_bulk(stream, b"server")?;
    write_bulk(stream, b"yyds")?;
    write_bulk(stream, b"version")?;
    write_bulk(stream, env!("CARGO_PKG_VERSION").as_bytes())?;
    write_bulk(stream, b"proto")?;
    write!(stream, ":{protocol}\r\n")?;
    write_bulk(stream, b"id")?;
    stream.write_all(b":0\r\n")?;
    write_bulk(stream, b"mode")?;
    write_bulk(stream, b"standalone")?;
    write_bulk(stream, b"role")?;
    write_bulk(stream, b"master")?;
    write_bulk(stream, b"modules")?;
    stream.write_all(b"*0\r\n")
}
