//! Redis 的事务会话语义：`MULTI` 队列、`WATCH` 版本与 `EXEC` 的入队校验。
//!
//! 状态按连接持有（队列与 watch 列表都属于单条连接），但语义归命令执行层：谁可以被
//! 排队、`EXEC` 如何判定 watch 失效、`DISCARD` 清掉什么，都由这里决定。连接层只负责
//! 套接字、协议版本与 `CLIENT LIST` 投影。

use database_protocol_redis::bind::{BindError, bind};
use database_protocol_redis::codec::CommandError;
use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};
use database_types::cluster::Namespace;

use super::{KeyValueExecutor, collections};

/// 一份参数已复制进自有缓冲的请求，供 `MULTI` 队列跨帧保存命令。
#[derive(Debug)]
pub struct OwnedRequest {
    arguments: Vec<Vec<u8>>,
}

impl OwnedRequest {
    /// 复制一帧参数。
    pub fn new(arguments: &[&[u8]]) -> Self {
        Self { arguments: arguments.iter().map(|argument| argument.to_vec()).collect() }
    }

    /// 把自有参数重新借成一次请求。
    pub fn as_request(&self) -> database_protocol_redis::resp::Request<'_> {
        database_protocol_redis::resp::Request { arguments: self.arguments.iter().map(|value| value.as_slice()).collect(), consumed: 0 }
    }

    /// 交出自有参数的所有权，供宿主在 `EXEC` 时逐条复放。
    pub fn into_arguments(self) -> Vec<Vec<u8>> {
        self.arguments
    }
}

/// `MULTI` 块内缓冲的一条命令。
#[derive(Debug)]
pub struct QueuedCommand {
    /// 被缓冲的命令参数。
    pub request: OwnedRequest,
}

/// `WATCH` 钉住的一个键与它当时的记录修订号。
///
/// 修订号就是乐观锁的版本：`EXEC` 通过键值合同重读它，任何差异都放弃整个事务。每次成功
/// 写入都会分配新的分片单调修订号，因此逐字节相同的重写、ABA 覆写与过期转换（记录消失、
/// 版本读成 `None`）都可见，而按值比较发现不了这些。
#[derive(Debug, Clone)]
pub struct WatchedKey {
    /// 取 watch 时的命名空间（`SELECT` 目标）；watch 按逻辑库隔离，与真机一致。
    pub namespace: Namespace,
    /// 不透明键字节。
    pub key: Vec<u8>,
    /// 取 watch 时观察到的修订号（`None` 表示键不存在）。
    pub revision: Option<u64>,
}

/// 一条连接的事务状态。
#[derive(Debug, Default)]
pub struct TransactionSession {
    in_multi: bool,
    queue: Vec<QueuedCommand>,
    watched: Vec<WatchedKey>,
    multi_error: bool,
    defer_blocking: bool,
}

impl TransactionSession {
    /// 新建一个空会话。
    pub fn new() -> Self {
        Self::default()
    }

    /// 新建一个把阻塞命令按非阻塞语义复放的会话。
    ///
    /// `EXEC` 复放的排队命令用它：真机把排队的 `BLPOP` 当非阻塞变体执行并立刻回 nil，
    /// 在 `EXEC` 内等待还会阻塞持有事务的连接。
    pub fn deferred() -> Self {
        Self { defer_blocking: true, ..Self::default() }
    }

    /// 连接是否处于 `MULTI` 块内。
    pub fn in_multi(&self) -> bool {
        self.in_multi
    }

    /// 队列里是否有被拒绝过的命令（事务已中毒）。
    pub fn multi_error(&self) -> bool {
        self.multi_error
    }

    /// 阻塞命令是否必须立即返回。
    pub fn defer_blocking(&self) -> bool {
        self.defer_blocking
    }

    /// 进入 `MULTI`，清空上一轮的队列与中毒标记。
    pub fn begin_multi(&mut self) {
        self.in_multi = true;
        self.queue.clear();
        self.multi_error = false;
    }

    /// 标记队列入队失败：`EXEC` 必须整队丢弃。
    pub fn poison(&mut self) {
        self.multi_error = true;
    }

    /// 缓冲一条命令，返回当前队列长度。
    pub fn queue(&mut self, arguments: &[&[u8]]) -> usize {
        self.queue.push(QueuedCommand { request: OwnedRequest::new(arguments) });
        self.queue.len()
    }

    /// 丢弃事务、队列与 watch 列表，回到非事务状态。
    pub fn discard(&mut self) {
        self.in_multi = false;
        self.queue.clear();
        self.watched.clear();
        self.multi_error = false;
    }

    /// 取出队列并结束事务；`EXEC` 无论队列是否为空都会释放 watch。
    pub fn finish_exec(&mut self) -> Vec<QueuedCommand> {
        self.in_multi = false;
        self.multi_error = false;
        self.watched.clear();
        std::mem::take(&mut self.queue)
    }

    /// 已钉住的键，按首次 watch 顺序排列。
    pub fn watched(&self) -> &[WatchedKey] {
        &self.watched
    }

    /// 该键是否已经被钉住（重复 `WATCH` 保留最初修订号）。
    pub fn is_watching(&self, namespace: &Namespace, key: &[u8]) -> bool {
        self.watched.iter().any(|entry| entry.namespace == *namespace && entry.key.as_slice() == key)
    }

    /// 钉住一个键的修订号。
    pub fn watch(&mut self, namespace: &Namespace, key: &[u8], revision: Option<u64>) {
        self.watched.push(WatchedKey { namespace: namespace.clone(), key: key.to_vec(), revision });
    }

    /// 释放全部钉住的键，不影响队列。
    pub fn unwatch(&mut self) {
        self.watched.clear();
    }
}

/// 读取一个键当前的记录修订号，即 `WATCH` 比较的版本。
pub fn key_revision(executor: &dyn KeyValueExecutor, namespace: &Namespace, key: &[u8]) -> Result<Option<u64>, String> {
    match executor.execute(None, KeyValueCommand { namespace: namespace.clone(), key: key.to_vec(), action: KeyValueAction::GetRevision }) {
        Ok(KeyValueResult::Revision(revision)) => Ok(revision),
        Ok(_) => Err("revision executor returned unexpected result".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 逐个比较 watch 时的修订号与当前修订号。
///
/// `true` 表示至少一个被观察的键在取 watch 之后被写过（或过期），调用方必须放弃事务
/// 而不是执行它。逐字节相同的重写同样推进修订号，因此在这里可见。
pub fn watched_keys_modified(executor: &dyn KeyValueExecutor, watched: &[WatchedKey]) -> Result<bool, String> {
    for entry in watched {
        let current = key_revision(executor, &entry.namespace, &entry.key)?;
        if current != entry.revision {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 入队前对每条命令做一次轻量的 arity / 语法检查。
///
/// 这里返回的错误会立刻写回客户端（不延迟到 `EXEC`）。语法不通过的命令按真机语义毒化
/// 整个事务，`EXEC` 随后回 `EXECABORT`。返回 `Err(CommandError)` 表示要发出的语义错误，
/// RESP error 行的字节形状统一由 `database_protocol_redis::codec` 编码。
pub fn validate_command_for_queue(
    command: &[u8],
    count: usize,
    arguments: &[&[u8]],
    has_executor: bool,
    namespace: &Namespace,
) -> Result<(), CommandError> {
    // PING: 1 or 2 args
    if command.eq_ignore_ascii_case(b"PING") {
        if count > 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'ping' command".to_owned()));
        }
        return Ok(());
    }
    // ECHO: exactly 2
    if command.eq_ignore_ascii_case(b"ECHO") {
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'echo' command".to_owned()));
        }
        return Ok(());
    }
    // QUIT: exactly 1
    if command.eq_ignore_ascii_case(b"QUIT") {
        if count != 1 {
            return Err(CommandError::Failed("wrong number of arguments for 'quit' command".to_owned()));
        }
        return Ok(());
    }
    // AUTH: password or username + password. Upstream queues it and answers the
    // no-password error at EXEC, so queueing only checks arity here.
    if command.eq_ignore_ascii_case(b"AUTH") {
        if !matches!(count, 2 | 3) {
            return Err(CommandError::Failed("wrong number of arguments for 'auth' command".to_owned()));
        }
        return Ok(());
    }
    // SELECT: exactly 2, needs executor
    if command.eq_ignore_ascii_case(b"SELECT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'select' command".to_owned()));
        }
        if parse_database(arguments[1]).is_none() {
            return Err(CommandError::Failed("DB index is out of range".to_owned()));
        }
        return Ok(());
    }
    // EXPIRE / PEXPIRE: at least 3, need executor, integer check. Conditional options
    // are validated when the command runs, like upstream.
    if command.eq_ignore_ascii_case(b"EXPIRE") || command.eq_ignore_ascii_case(b"PEXPIRE") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'expire' command".to_owned()));
        }
        if parse_integer(arguments[2]).is_none() {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        }
        return Ok(());
    }
    // EXPIREAT / PEXPIREAT: at least 3, need executor, integer check.
    if command.eq_ignore_ascii_case(b"EXPIREAT") || command.eq_ignore_ascii_case(b"PEXPIREAT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'expireat' command".to_owned()));
        }
        if parse_integer(arguments[2]).is_none() {
            return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
        }
        return Ok(());
    }
    // RENAME / RENAMENX: exactly 3, need executor. Source presence and destination
    // conflicts are runtime semantics and stay deferred to EXEC.
    if command.eq_ignore_ascii_case(b"RENAME") || command.eq_ignore_ascii_case(b"RENAMENX") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 3 {
            let name = if command.eq_ignore_ascii_case(b"RENAMENX") { "renamenx" } else { "rename" };
            return Err(CommandError::Failed(format!("wrong number of arguments for '{name}' command")));
        }
        return Ok(());
    }
    // GETEX: at least 2, needs executor, at most one well-formed expiry option
    if command.eq_ignore_ascii_case(b"GETEX") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'getex' command".to_owned()));
        }
        let mut seen_option = false;
        let mut index = 2;
        while index < count {
            let option = arguments[index];
            if seen_option {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            if option.eq_ignore_ascii_case(b"PERSIST") {
                seen_option = true;
                index += 1;
            }
            else if option.eq_ignore_ascii_case(b"EX")
                || option.eq_ignore_ascii_case(b"PX")
                || option.eq_ignore_ascii_case(b"EXAT")
                || option.eq_ignore_ascii_case(b"PXAT")
            {
                let Some(raw) = arguments.get(index + 1)
                else {
                    return Err(CommandError::Failed("syntax error".to_owned()));
                };
                if parse_integer(raw).is_none() {
                    return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
                }
                seen_option = true;
                index += 2;
            }
            else {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
        }
        return Ok(());
    }
    // OBJECT: at least a subcommand, needs executor. The subcommand name and the key
    // argument are runtime checks and stay deferred to EXEC, like upstream.
    if command.eq_ignore_ascii_case(b"OBJECT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'object' command".to_owned()));
        }
        return Ok(());
    }
    // TTL / PTTL / TYPE / STRLEN: exactly 2
    if command.eq_ignore_ascii_case(b"TTL")
        || command.eq_ignore_ascii_case(b"PTTL")
        || command.eq_ignore_ascii_case(b"TYPE")
        || command.eq_ignore_ascii_case(b"STRLEN")
    {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments".to_owned()));
        }
        return Ok(());
    }
    // MGET: at least 2
    if command.eq_ignore_ascii_case(b"MGET") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'mget' command".to_owned()));
        }
        return Ok(());
    }
    // EXISTS: at least 2
    if command.eq_ignore_ascii_case(b"EXISTS") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'exists' command".to_owned()));
        }
        return Ok(());
    }
    // APPEND: exactly 3
    if command.eq_ignore_ascii_case(b"APPEND") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'append' command".to_owned()));
        }
        return Ok(());
    }
    // PERSIST: exactly 2
    if command.eq_ignore_ascii_case(b"PERSIST") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'persist' command".to_owned()));
        }
        return Ok(());
    }
    // SETEX: exactly 4 with a positive integer TTL
    if command.eq_ignore_ascii_case(b"SETEX") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 4 {
            return Err(CommandError::Failed("wrong number of arguments for 'setex' command".to_owned()));
        }
        if parse_integer(arguments[2]).is_none_or(|seconds| seconds <= 0) {
            return Err(CommandError::Failed("invalid expire time in 'setex' command".to_owned()));
        }
        return Ok(());
    }
    // CLIENT: only the session-local subcommands are queueable.
    if command.eq_ignore_ascii_case(b"CLIENT") {
        let Some(subcommand) = arguments.get(1)
        else {
            return Err(CommandError::Failed("Unknown subcommand or wrong number of arguments for 'CLIENT'. Try CLIENT HELP.".to_owned()));
        };
        if subcommand.eq_ignore_ascii_case(b"SETINFO") {
            let valid = count == 4 && (arguments[2].eq_ignore_ascii_case(b"LIB-NAME") || arguments[2].eq_ignore_ascii_case(b"LIB-VER"));
            if !valid {
                return Err(CommandError::Failed("wrong number of arguments for 'client|setinfo' command".to_owned()));
            }
            return Ok(());
        }
        if subcommand.eq_ignore_ascii_case(b"SETNAME") {
            if count != 3 {
                return Err(CommandError::Failed("wrong number of arguments for 'client|setname' command".to_owned()));
            }
            return Ok(());
        }
        if subcommand.eq_ignore_ascii_case(b"GETNAME") {
            if count != 2 {
                return Err(CommandError::Failed("wrong number of arguments for 'client|getname' command".to_owned()));
            }
            return Ok(());
        }
        return Err(CommandError::Failed("unsupported command".to_owned()));
    }
    // Hash / list value commands: they need the executor. Their arity and type checks
    // are runtime semantics and stay deferred to EXEC, like upstream queueing.
    if collections::is_hash_command(command)
        || collections::is_list_command(command)
        || collections::is_set_command(command)
        || collections::is_zset_command(command)
        || collections::is_hyperloglog_command(command)
    {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        return Ok(());
    }
    // RANDOMKEY / DBSIZE: exactly 1. FLUSHALL accepts an optional ASYNC / SYNC.
    if command.eq_ignore_ascii_case(b"RANDOMKEY") || command.eq_ignore_ascii_case(b"DBSIZE") || command.eq_ignore_ascii_case(b"FLUSHALL") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        let maximum = if command.eq_ignore_ascii_case(b"FLUSHALL") { 2 } else { 1 };
        if count > maximum {
            return Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", lowercase_command(command))));
        }
        return Ok(());
    }
    // COPY: at least 3, needs executor. Options are validated at runtime.
    if command.eq_ignore_ascii_case(b"COPY") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 3 {
            return Err(CommandError::Failed("wrong number of arguments for 'copy' command".to_owned()));
        }
        return Ok(());
    }
    // EXPIRETIME / PEXPIRETIME: exactly 2.
    if command.eq_ignore_ascii_case(b"EXPIRETIME") || command.eq_ignore_ascii_case(b"PEXPIRETIME") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 2 {
            return Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", lowercase_command(command))));
        }
        return Ok(());
    }
    // SCAN: cursor iteration, options are validated at runtime like upstream.
    if command.eq_ignore_ascii_case(b"SCAN") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'scan' command".to_owned()));
        }
        return Ok(());
    }
    // KEYS: exactly one pattern, needs executor.
    if command.eq_ignore_ascii_case(b"KEYS") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'keys' command".to_owned()));
        }
        return Ok(());
    }
    // DEL: one or more keys, mirrors the `-2` arity of the upstream command.
    if command.eq_ignore_ascii_case(b"DEL") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count < 2 {
            return Err(CommandError::Failed("wrong number of arguments for 'del' command".to_owned()));
        }
        return Ok(());
    }
    // String range and bit commands: they need the executor. Argument values are
    // validated at runtime, so queueing only checks arity like upstream.
    if command.eq_ignore_ascii_case(b"GETRANGE") || command.eq_ignore_ascii_case(b"SETRANGE") || command.eq_ignore_ascii_case(b"SETBIT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 4 {
            return Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", lowercase_command(command))));
        }
        return Ok(());
    }
    if command.eq_ignore_ascii_case(b"INCRBYFLOAT") || command.eq_ignore_ascii_case(b"GETBIT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if count != 3 {
            return Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", lowercase_command(command))));
        }
        return Ok(());
    }
    if command.eq_ignore_ascii_case(b"BITCOUNT") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if !matches!(count, 2 | 4 | 5) {
            return Err(CommandError::Failed("wrong number of arguments for 'bitcount' command".to_owned()));
        }
        return Ok(());
    }
    if command.eq_ignore_ascii_case(b"BITPOS") {
        if !has_executor {
            return Err(CommandError::Failed("unsupported command".to_owned()));
        }
        if !(3..=6).contains(&count) {
            return Err(CommandError::Failed("wrong number of arguments for 'bitpos' command".to_owned()));
        }
        return Ok(());
    }
    // Fall back to bind()'s own syntax checks.
    match bind(arguments, namespace.0.as_str()) {
        Ok(_) => Ok(()),
        Err(BindError::WrongArity) => Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", lowercase_command(command)))),
        Err(BindError::Syntax) => Err(CommandError::Failed("syntax error".to_owned())),
        Err(BindError::UnsupportedOptions) => Err(CommandError::Failed("syntax error".to_owned())),
        Err(BindError::InvalidInteger) => Err(CommandError::Failed("value is not an integer or out of range".to_owned())),
        Err(BindError::InvalidExpire) => Err(CommandError::Failed("invalid expire time in 'set' command".to_owned())),
        Err(BindError::EmptyCommand) => Err(CommandError::Failed("empty command".to_owned())),
        Err(BindError::UnsupportedCommand) => Err(CommandError::Failed("unsupported command".to_owned())),
    }
}

/// 解析逻辑库序号：只接受规范十进制且小于数据库个数。
pub fn parse_database(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let database = bytes.iter().try_fold(0_u8, |value, digit| value.checked_mul(10)?.checked_add(digit - b'0'))?;
    (database < 16).then_some(database)
}

/// 解析规范十进制整数：只接受能原样往返的写法。
pub fn parse_integer(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value = text.parse::<i64>().ok()?;
    // 拒绝非规范写法（`+1`、` 1` 等）：只有原样往返的十进制才接受。
    if value.to_string() != text {
        return None;
    }
    Some(value)
}

/// 命令名转小写，用于构造 `wrong number of arguments for '<name>' command`。
pub fn lowercase_command(command: &[u8]) -> String {
    String::from_utf8_lossy(command).to_ascii_lowercase()
}
