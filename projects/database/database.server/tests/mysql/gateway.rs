//! MySQL wire 口径：握手协商的能力必须与 OK 包里的 affected rows 一致。
//!
//! `CLIENT_FOUND_ROWS`（0x02）是客户端在握手响应里声明的能力位。协商后 `UPDATE` 的
//! OK 包报告**命中行数**，未协商时报告**改变行数**——两者不能混用，否则驱动看到的
//! 行数与真实写入不一致。

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use database_cluster::topology::ShardMap;
use database_execution::{
    cluster::{KeyValueExecutor, LocalExecutor},
    mysql::MysqlSqlRuntime,
    sql_schema::SqlSchemaRegistry,
};
use database_server::yyds::mysql::{DEFAULT_PORT, GATEWAY_ID, GATEWAY_KIND, connection::serve_connection, startup_banner};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

#[test]
fn mysql_gateway_metadata_is_stable() {
    assert_eq!(GATEWAY_ID, "mysql");
    assert_eq!(DEFAULT_PORT, 3306);
    assert_eq!(GATEWAY_KIND.id(), GATEWAY_ID);
    assert!(startup_banner(DEFAULT_PORT).contains("3306"));
}

const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const COM_QUERY: u8 = 0x03;
const COM_QUIT: u8 = 0x01;

struct Runtime {
    address: String,
    directory: PathBuf,
    worker: Option<thread::JoinHandle<()>>,
}

impl Runtime {
    fn start(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("database-server-mysql-wire-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let shard = ShardId("local-0".into());
        let executor: Arc<dyn KeyValueExecutor> = Arc::new(
            LocalExecutor::new(
                ShardMap::new(ShardEpoch(1), vec![shard.clone()]).unwrap(),
                HashMap::from([(shard, FileShard::open(directory.join("local-0.yykv")).unwrap())]),
                Vec::new(),
            )
            .unwrap(),
        );
        let runtime = Arc::new(MysqlSqlRuntime { schema: Arc::new(SqlSchemaRegistry::new_in_memory()), executor });
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let worker = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = serve_connection(stream, Some(runtime));
        });
        Self { address, directory, worker: Some(worker) }
    }

    fn connect(&self) -> Wire {
        Wire::start(&self.address)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// 一条最小 classic-protocol 会话，只覆盖握手、`COM_QUERY` 与 OK 包。
struct Wire {
    stream: TcpStream,
}

impl Wire {
    fn start(address: &str) -> Self {
        let stream = TcpStream::connect(address).expect("connect gateway");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
        stream.set_nodelay(true).expect("nodelay");
        let mut wire = Self { stream };
        let (_, greeting) = wire.read_packet();
        assert_eq!(greeting.first(), Some(&10), "the gateway must open with a MySQL v10 greeting");
        wire
    }

    /// Performs the startup handshake with `capabilities` and waits for the OK.
    fn handshake(&mut self, capabilities: u32) {
        let mut payload = Vec::with_capacity(32 + 8);
        payload.extend_from_slice(&capabilities.to_le_bytes());
        payload.extend_from_slice(&(1024_u32 * 1024).to_le_bytes());
        payload.push(45); // utf8mb4_general_ci
        payload.extend_from_slice(&[0; 23]);
        payload.extend_from_slice(b"root\0");
        payload.push(0); // empty auth response (loopback accepts no password)
        self.write_packet(1, &payload);
        let (sequence, ok) = self.read_packet();
        assert_eq!(sequence, 2, "the handshake must be answered with sequence 2");
        assert_eq!(ok.first(), Some(&0x00), "the handshake must be accepted");
    }

    /// Sends one statement and returns the affected rows the OK packet carries.
    fn affected_rows(&mut self, sql: &str) -> u64 {
        let mut payload = Vec::with_capacity(1 + sql.len());
        payload.push(COM_QUERY);
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &payload);
        let (_, reply) = self.read_packet();
        assert_eq!(reply.first(), Some(&0x00), "`{sql}` must answer with an OK packet");
        read_length_encoded(&reply[1..])
    }

    fn write_packet(&mut self, sequence: u8, payload: &[u8]) {
        let mut frame = Vec::with_capacity(4 + payload.len());
        let length = payload.len() as u32;
        frame.extend_from_slice(&length.to_le_bytes()[..3]);
        frame.push(sequence);
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame).expect("write packet");
    }

    fn read_packet(&mut self) -> (u8, Vec<u8>) {
        let mut header = [0_u8; 4];
        self.stream.read_exact(&mut header).expect("read packet header");
        let length = usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0_u8; length];
        self.stream.read_exact(&mut payload).expect("read packet payload");
        (header[3], payload)
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.stream.write_all(&[1, 0, 0, 0, COM_QUIT]);
    }
}

/// Decodes a length-encoded integer from the start of `bytes`.
fn read_length_encoded(bytes: &[u8]) -> u64 {
    match bytes.first() {
        Some(&value) if value < 0xFB => u64::from(value),
        Some(&0xFC) => u64::from(u16::from_le_bytes([bytes[1], bytes[2]])),
        Some(&0xFD) => u64::from(bytes[1]) | (u64::from(bytes[2]) << 8) | (u64::from(bytes[3]) << 16),
        Some(&0xFE) => u64::from_le_bytes(bytes[1..9].try_into().expect("eight bytes")),
        other => panic!("unexpected length-encoded integer prefix: {other:?}"),
    }
}

/// 未协商 `CLIENT_FOUND_ROWS`：OK 包按改变行数计，写回同值是 0。
#[test]
fn update_ok_packet_reports_changed_rows_by_default() {
    let runtime = Runtime::start("changed-rows");
    let mut wire = runtime.connect();
    wire.handshake(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION);

    assert_eq!(wire.affected_rows("CREATE TABLE stock (id INT NOT NULL PRIMARY KEY, qty INT)"), 0);
    assert_eq!(wire.affected_rows("INSERT INTO stock (id, qty) VALUES (1, 5)"), 1);
    assert_eq!(wire.affected_rows("UPDATE stock SET qty = 5 WHERE id = 1"), 0);
    assert_eq!(wire.affected_rows("UPDATE stock SET qty = 6 WHERE id = 1"), 1);


}

/// 协商 `CLIENT_FOUND_ROWS`：同一语句的 OK 包改成命中行数。
#[test]
fn found_rows_capability_switches_the_ok_packet_to_matched_rows() {
    let runtime = Runtime::start("found-rows");
    let mut wire = runtime.connect();
    wire.handshake(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_FOUND_ROWS);

    assert_eq!(wire.affected_rows("CREATE TABLE stock (id INT NOT NULL PRIMARY KEY, qty INT)"), 0);
    assert_eq!(wire.affected_rows("INSERT INTO stock (id, qty) VALUES (1, 5), (2, 7)"), 2);
    // 写回同值：改变 0 行，命中 1 行。
    assert_eq!(wire.affected_rows("UPDATE stock SET qty = 5 WHERE id = 1"), 1);
    // 两条都命中，只有一条真正改变。
    assert_eq!(wire.affected_rows("UPDATE stock SET qty = 5 WHERE id > 0"), 2);


}
