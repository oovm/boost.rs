//! Real RESP sessions against a shard-backed executor.
//!
//! 这些用例走完整套接字读循环（`serve_cancellable_with_executor_and_tracker`），而不是只
//! 调用命令处理函数，因此 `MULTI` / `EXEC` / `WATCH` 的会话状态、回复分帧与并发连接都按
//! 真实会话验证。

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use database_execution::cluster::{
    KeyValueExecutor,
    redis::{RedisExecutor, blocking::BlockingRegistry, string::KeyAccessTracker},
};
use database_query::cluster_kv::{KeyValueCommand, KeyValueResult};
use database_server::yyds::redis::{
    connection::serve_cancellable_with_executor_and_tracker, pubsub::PubSubRegistry, resp::RequestLimits, server_face::ClientRegistry,
};
use database_storage::cluster_kv::redis::RedisShard;
use database_types::cluster::{FenceToken, Namespace};

/// 一条 RESP 回复的解码结果，只覆盖连接层会产出的形状。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Value>>),
    Double(String),
}

impl Value {
    fn bulk(&self) -> String {
        match self {
            Self::Bulk(Some(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("expected a bulk string, got {other:?}"),
        }
    }
}

/// 服务器面用 `redis:<db>[:kind]` 命名空间，而 `RedisExecutor` 用 `redis:db<db>[:kind]`。
///
/// 生产宿主自己做这层映射；测试里用一个薄适配器复用同一份分片执行器。
#[derive(Debug)]
struct GatewayExecutor(RedisExecutor<'static>);

impl GatewayExecutor {
    fn rewrite(namespace: &Namespace) -> Namespace {
        match namespace.0.strip_prefix("redis:") {
            Some(rest) if !rest.starts_with("db") => Namespace(format!("redis:db{rest}")),
            _ => namespace.clone(),
        }
    }
}

impl KeyValueExecutor for GatewayExecutor {
    fn execute(&self, fence: Option<&FenceToken>, command: KeyValueCommand) -> database_types::cluster::Result<KeyValueResult> {
        let namespace = Self::rewrite(&command.namespace);
        self.0.execute(fence, KeyValueCommand { namespace, ..command })
    }

    fn execute_scan(
        &self,
        fence: Option<&FenceToken>,
        namespace: &Namespace,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> database_types::cluster::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.0.execute_scan(fence, &Self::rewrite(namespace), prefix, after, before, limit)
    }
}

/// 一个监听器加一份临时分片，测试结束时删除目录。
struct Gateway {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    data_dir: PathBuf,
}

impl Gateway {
    fn start(name: &str) -> Self {
        let data_dir = std::env::temp_dir().join(format!("redis-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // 执行器借用一个活到进程结束的分片：监听器线程与用例线程共享同一把锁。
        let shard: &'static Mutex<RedisShard> = Box::leak(Box::new(Mutex::new(RedisShard::open(&data_dir).expect("open shard"))));
        let executor: Arc<dyn KeyValueExecutor> = Arc::new(GatewayExecutor(RedisExecutor::new(shard)));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let stopping = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(KeyAccessTracker::new());
        let registry = Arc::new(BlockingRegistry::new());
        let pubsub = Arc::new(PubSubRegistry::new());
        let clients = Arc::new(ClientRegistry::new(address.port(), "127.0.0.1".to_string()));
        let signal = Arc::clone(&stopping);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if signal.load(Ordering::Acquire) {
                    return;
                }
                let Ok(stream) = stream
                else {
                    continue;
                };
                let executor = Arc::clone(&executor);
                let tracker = Arc::clone(&tracker);
                let registry = Arc::clone(&registry);
                let pubsub = Arc::clone(&pubsub);
                let clients = Arc::clone(&clients);
                let cancellation = Arc::clone(&signal);
                thread::spawn(move || {
                    let _ = serve_cancellable_with_executor_and_tracker(
                        stream,
                        RequestLimits::default(),
                        cancellation,
                        executor,
                        tracker,
                        registry,
                        pubsub,
                        Some(clients),
                    );
                });
            }
        });
        Self { address, stopping, data_dir }
    }

    fn connect(&self) -> Session {
        let stream = TcpStream::connect(self.address).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
        stream.set_nodelay(true).expect("nodelay");
        let reader = BufReader::new(stream.try_clone().expect("clone socket"));
        Session { reader, writer: stream }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// 一条 RESP2 会话。
struct Session {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Session {
    fn send(&mut self, arguments: &[&str]) {
        let mut frame = format!("*{}\r\n", arguments.len()).into_bytes();
        for argument in arguments {
            frame.extend_from_slice(format!("${}\r\n", argument.len()).as_bytes());
            frame.extend_from_slice(argument.as_bytes());
            frame.extend_from_slice(b"\r\n");
        }
        self.writer.write_all(&frame).expect("write request");
        self.writer.flush().expect("flush request");
    }

    fn receive(&mut self) -> Value {
        read_value(&mut self.reader)
    }

    fn call(&mut self, arguments: &[&str]) -> Value {
        self.send(arguments);
        self.receive()
    }

    /// 发一条命令并断言它回 `+OK`。
    fn ok(&mut self, arguments: &[&str]) {
        assert_eq!(self.call(arguments), Value::Simple("OK".to_string()), "{arguments:?}");
    }
}

fn read_line(reader: &mut BufReader<TcpStream>) -> String {
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).expect("read reply line");
    while line.last().is_some_and(|byte| matches!(byte, b'\r' | b'\n')) {
        line.pop();
    }
    String::from_utf8(line).expect("utf-8 reply line")
}

fn read_value(reader: &mut BufReader<TcpStream>) -> Value {
    let line = read_line(reader);
    let (prefix, rest) = line.split_at(1);
    match prefix {
        "+" => Value::Simple(rest.to_string()),
        "-" => Value::Error(rest.to_string()),
        ":" => Value::Integer(rest.parse().expect("integer reply")),
        "," => Value::Double(rest.to_string()),
        "$" => {
            let length: i64 = rest.parse().expect("bulk length");
            if length < 0 {
                return Value::Bulk(None);
            }
            let mut bytes = vec![0_u8; length as usize + 2];
            reader.read_exact(&mut bytes).expect("bulk body");
            bytes.truncate(length as usize);
            Value::Bulk(Some(bytes))
        }
        "*" => {
            let count: i64 = rest.parse().expect("array length");
            if count < 0 {
                return Value::Array(None);
            }
            Value::Array(Some((0..count).map(|_| read_value(reader)).collect()))
        }
        other => panic!("unexpected reply prefix {other:?}"),
    }
}

#[test]
fn a_set_write_invalidates_the_watch_of_its_key() {
    let gateway = Gateway::start("watch-set");
    let mut client = gateway.connect();
    assert_eq!(client.call(&["SADD", "myset", "a"]), Value::Integer(1));
    client.ok(&["WATCH", "myset"]);
    // `SADD` 落在 `set` 种类命名空间上：`EXEC` 必须看见这次写入。
    assert_eq!(client.call(&["SADD", "myset", "b"]), Value::Integer(1));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["SADD", "myset", "c"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(None), "a set write must abort the transaction");
}

#[test]
fn a_repeated_member_add_leaves_the_watch_intact() {
    let gateway = Gateway::start("watch-set-noop");
    let mut client = gateway.connect();
    assert_eq!(client.call(&["SADD", "myset", "a"]), Value::Integer(1));
    client.ok(&["WATCH", "myset"]);
    // 重复加入已存在成员是空操作：真机不把它算作修改，事务仍然执行。
    assert_eq!(client.call(&["SADD", "myset", "a"]), Value::Integer(0));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["SADD", "myset", "b"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(Some(vec![Value::Integer(1)])), "a no-op add must not abort");
    assert_eq!(client.call(&["SCARD", "myset"]), Value::Integer(2));
}

#[test]
fn a_hash_or_list_write_invalidates_the_watch_of_its_key() {
    let gateway = Gateway::start("watch-collections");
    let mut client = gateway.connect();

    client.ok(&["WATCH", "myhash"]);
    assert_eq!(client.call(&["HSET", "myhash", "field", "value"]), Value::Integer(1));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["HSET", "myhash", "other", "value"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(None), "a hash write must abort the transaction");

    client.ok(&["WATCH", "mylist"]);
    assert_eq!(client.call(&["LPUSH", "mylist", "one"]), Value::Integer(1));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["LPUSH", "mylist", "two"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(None), "a list write must abort the transaction");
}

#[test]
fn a_ttl_write_on_a_collection_invalidates_the_watch_of_its_key() {
    let gateway = Gateway::start("watch-ttl");
    let mut client = gateway.connect();
    assert_eq!(client.call(&["SADD", "myset", "a"]), Value::Integer(1));
    client.ok(&["WATCH", "myset"]);
    // TTL 写落在集合自己的种类命名空间上，只看字符串种类的版本是看不见的。
    assert_eq!(client.call(&["EXPIRE", "myset", "100"]), Value::Integer(1));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["SADD", "myset", "b"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(None), "a TTL write must abort the transaction");
}

#[test]
fn persist_without_a_deadline_leaves_the_watch_intact() {
    let gateway = Gateway::start("watch-persist");
    let mut client = gateway.connect();
    client.ok(&["SET", "plain", "value"]);
    client.ok(&["WATCH", "plain"]);
    // 没有截止时间的 `PERSIST` 什么也没改，真机不把它算作修改。
    assert_eq!(client.call(&["PERSIST", "plain"]), Value::Integer(0));
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["SET", "plain", "next"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(Some(vec![Value::Simple("OK".to_string())])), "a no-op persist must not abort");
}

#[test]
fn a_byte_identical_set_still_invalidates_the_watch_of_its_key() {
    let gateway = Gateway::start("watch-set-identical");
    let mut client = gateway.connect();
    client.ok(&["SET", "plain", "value"]);
    client.ok(&["WATCH", "plain"]);
    // 与 `SADD` 不同，`SET` 即使写入相同字节也算一次修改。
    client.ok(&["SET", "plain", "value"]);
    client.ok(&["MULTI"]);
    assert_eq!(client.call(&["SET", "plain", "next"]), Value::Simple("QUEUED".to_string()));
    assert_eq!(client.call(&["EXEC"]), Value::Array(None), "an identical SET is still a modification");
}

/// 轮询 `INFO clients`，等阻塞计数到达期望值。
fn wait_for_blocked(observer: &mut Session, expected: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let info = observer.call(&["INFO", "clients"]).bulk();
        let blocked = info.lines().find_map(|line| line.strip_prefix("blocked_clients:")).and_then(|rest| rest.trim().parse::<i64>().ok());
        if blocked == Some(expected) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "blocked_clients never reached {expected}: {info}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_blocked_client_is_projected_and_served_on_the_next_push() {
    let gateway = Gateway::start("blocked-projection");
    let mut blocked = gateway.connect();
    let mut producer = gateway.connect();
    let Value::Integer(blocked_id) = blocked.call(&["CLIENT", "ID"])
    else {
        panic!("CLIENT ID must answer an integer");
    };

    // 不读回复：这条连接停在 `BLPOP` 上。
    blocked.send(&["BLPOP", "jobs", "0"]);
    wait_for_blocked(&mut producer, 1);

    let info = producer.call(&["INFO", "clients"]).bulk();
    assert!(info.contains("blocked_clients:1"), "{info}");
    assert!(info.contains("total_blocking_keys:1"), "{info}");
    let list = producer.call(&["CLIENT", "LIST"]).bulk();
    let line =
        list.lines().find(|line| line.contains(&format!("id={blocked_id} "))).expect("the blocked connection must appear in CLIENT LIST");
    assert!(line.contains("flags=Nb"), "{line}");
    assert!(line.contains("cmd=blpop"), "{line}");

    assert_eq!(producer.call(&["LPUSH", "jobs", "work"]), Value::Integer(1));
    assert_eq!(
        blocked.receive(),
        Value::Array(Some(vec![Value::Bulk(Some(b"jobs".to_vec())), Value::Bulk(Some(b"work".to_vec()))])),
        "the pushed element must reach the first waiter"
    );
    wait_for_blocked(&mut producer, 0);
    let list = producer.call(&["CLIENT", "LIST"]).bulk();
    let line = list.lines().find(|line| line.contains(&format!("id={blocked_id} "))).expect("still connected");
    assert!(line.contains("flags=N "), "{line}");
}

#[test]
fn client_unblock_times_out_the_blocked_command_and_keeps_the_connection() {
    let gateway = Gateway::start("unblock-timeout");
    let mut blocked = gateway.connect();
    let mut peer = gateway.connect();
    let Value::Integer(blocked_id) = blocked.call(&["CLIENT", "ID"])
    else {
        panic!("CLIENT ID must answer an integer");
    };
    blocked.send(&["BLPOP", "jobs", "0"]);
    wait_for_blocked(&mut peer, 1);

    assert_eq!(peer.call(&["CLIENT", "UNBLOCK", &blocked_id.to_string()]), Value::Integer(1));
    assert_eq!(blocked.receive(), Value::Array(None), "TIMEOUT unblocks with the empty reply");
    // 解除阻塞后连接继续可用，且不再计入阻塞客户端。
    assert_eq!(blocked.call(&["PING"]), Value::Simple("PONG".to_string()));
    wait_for_blocked(&mut peer, 0);
    assert_eq!(peer.call(&["CLIENT", "UNBLOCK", &blocked_id.to_string()]), Value::Integer(0), "an idle client cannot be unblocked");
}

#[test]
fn client_unblock_error_reports_the_unblocked_error() {
    let gateway = Gateway::start("unblock-error");
    let mut blocked = gateway.connect();
    let mut peer = gateway.connect();
    let Value::Integer(blocked_id) = blocked.call(&["CLIENT", "ID"])
    else {
        panic!("CLIENT ID must answer an integer");
    };
    blocked.send(&["BLPOP", "jobs", "0"]);
    wait_for_blocked(&mut peer, 1);

    assert_eq!(peer.call(&["CLIENT", "UNBLOCK", &blocked_id.to_string(), "ERROR"]), Value::Integer(1));
    match blocked.receive() {
        Value::Error(message) => assert_eq!(message, "UNBLOCKED client unblocked via CLIENT UNBLOCK"),
        other => panic!("ERROR unblock must answer an error, got {other:?}"),
    }
    assert_eq!(blocked.call(&["PING"]), Value::Simple("PONG".to_string()));
}

#[test]
fn blockers_on_one_key_are_served_in_registration_order() {
    let gateway = Gateway::start("blocked-fifo");
    let mut first = gateway.connect();
    let mut second = gateway.connect();
    let mut producer = gateway.connect();

    first.send(&["BLPOP", "jobs", "0"]);
    wait_for_blocked(&mut producer, 1);
    second.send(&["BLPOP", "jobs", "0"]);
    wait_for_blocked(&mut producer, 2);
    assert!(producer.call(&["INFO", "clients"]).bulk().contains("total_blocking_keys:1"), "one key, two waiters");

    assert_eq!(producer.call(&["LPUSH", "jobs", "one"]), Value::Integer(1));
    assert_eq!(
        first.receive(),
        Value::Array(Some(vec![Value::Bulk(Some(b"jobs".to_vec())), Value::Bulk(Some(b"one".to_vec()))])),
        "the earlier waiter must win the first element"
    );
    assert_eq!(producer.call(&["LPUSH", "jobs", "two"]), Value::Integer(1));
    assert_eq!(
        second.receive(),
        Value::Array(Some(vec![Value::Bulk(Some(b"jobs".to_vec())), Value::Bulk(Some(b"two".to_vec()))])),
        "the later waiter must take the second element"
    );
}

#[test]
fn a_blocking_pop_answers_immediately_when_data_is_present() {
    let gateway = Gateway::start("blocked-ready");
    let mut client = gateway.connect();
    assert_eq!(client.call(&["RPUSH", "jobs", "work"]), Value::Integer(1));
    assert_eq!(
        client.call(&["BLPOP", "jobs", "0"]),
        Value::Array(Some(vec![Value::Bulk(Some(b"jobs".to_vec())), Value::Bulk(Some(b"work".to_vec()))])),
        "data already in the list must be served without waiting"
    );
}

#[test]
fn a_blocking_sorted_set_pop_waits_for_a_member() {
    let gateway = Gateway::start("blocked-zset");
    let mut blocked = gateway.connect();
    let mut producer = gateway.connect();
    let Value::Integer(blocked_id) = blocked.call(&["CLIENT", "ID"])
    else {
        panic!("CLIENT ID must answer an integer");
    };
    blocked.send(&["BZPOPMIN", "scores", "0"]);
    wait_for_blocked(&mut producer, 1);
    let list = producer.call(&["CLIENT", "LIST"]).bulk();
    let line = list.lines().find(|line| line.contains(&format!("id={blocked_id} "))).expect("the blocked connection must appear");
    assert!(line.contains("flags=Nb"), "{line}");
    assert!(line.contains("cmd=bzpopmin"), "{line}");

    assert_eq!(producer.call(&["ZADD", "scores", "1", "member"]), Value::Integer(1));
    match blocked.receive() {
        Value::Array(Some(values)) => {
            assert_eq!(values[0], Value::Bulk(Some(b"scores".to_vec())));
            assert_eq!(values[1], Value::Bulk(Some(b"member".to_vec())));
        }
        other => panic!("BZPOPMIN must answer the popped member, got {other:?}"),
    }
}

#[test]
fn a_blocking_command_answers_empty_once_its_timeout_elapses() {
    let gateway = Gateway::start("blocked-timeout");
    let mut blocked = gateway.connect();
    let mut peer = gateway.connect();
    blocked.send(&["BLPOP", "jobs", "0.4"]);
    wait_for_blocked(&mut peer, 1);
    assert_eq!(blocked.receive(), Value::Array(None), "an elapsed timeout answers the empty reply");
    wait_for_blocked(&mut peer, 0);
}
