//! Bounded loopback MySQL protocol-v10 startup and SQL execution.

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use database_execution::mysql::{
    MysqlPrepareOutcome, MysqlPreparedRegistry, MysqlPreparedStatement, MysqlSqlRuntime, MysqlStatementSink, TransactionSession,
    dispatch_query, execute_prepared, prepare_statement,
};
use database_protocol_mysql::{
    LenencError, MAX_PACKET_PAYLOAD, MYSQL_TYPE_BLOB, MYSQL_TYPE_DATE, MYSQL_TYPE_DATETIME, MYSQL_TYPE_DECIMAL, MYSQL_TYPE_DOUBLE,
    MYSQL_TYPE_FLOAT, MYSQL_TYPE_INT24, MYSQL_TYPE_LONG, MYSQL_TYPE_LONG_BLOB, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_MEDIUM_BLOB,
    MYSQL_TYPE_NEWDECIMAL, MYSQL_TYPE_NULL, MYSQL_TYPE_SHORT, MYSQL_TYPE_STRING, MYSQL_TYPE_TINY, MYSQL_TYPE_TINY_BLOB, MYSQL_TYPE_VAR_STRING,
    MYSQL_TYPE_YEAR, PROTOCOL_VERSION, append_packet, column_definition_payload, encode_length_encoded, eof_payload, error_payload, ok_payload,
    prepare_ok_payload, read_length_encoded,
};
use database_query::{MYSQL_BLOB_CAPACITY, SqlResult, SqlValue};

use super::wire::{PacketError, encode_packet};

const SERVER_CAPABILITIES: u32 = 0x0000_0001 | 0x0000_0004 | 0x0000_0008 | 0x0000_0200 | 0x0000_2000 | 0x0000_8000 | 0x0002_0000 | 0x0008_0000;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SSL: u32 = 0x0000_0800;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
const CLIENT_PLUGIN_AUTH_LENENC: u32 = 0x0020_0000;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// Largest request frame accepted, and the value advertised as
/// `@@max_allowed_packet`. Keeping the two equal means a client never sends
/// a frame the server will refuse.
const MAX_PACKET: usize = 1024 * 1024;

const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_PING: u8 = 0x0E;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1A;
const COM_RESET_CONNECTION: u8 = 0x1F;

/// `ER_UNKNOWN_COM_ERROR` — returned for every command byte the gateway does not
/// implement so the client sees a real MySQL error instead of a desynchronised
/// stream.
const ER_UNKNOWN_COM_ERROR: u16 = 1047;

/// Performs a loopback-only unauthenticated startup and executes SQL against the provided runtime.
pub fn serve_connection(stream: TcpStream, runtime: Option<Arc<MysqlSqlRuntime>>) -> io::Result<()> {
    if !stream.peer_addr()?.ip().is_loopback() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "MySQL requires loopback until authentication and TLS are implemented"));
    }
    // 结果集由多个小包依次写出，关闭 Nagle 让每包立即发出。留着 Nagle 时
    // Linux 会与对端的延迟 ACK 叠加出约 40ms 的往返停顿。
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    serve_session(stream, runtime)
}

/// Serves a loopback session that can be interrupted by the owning node.
///
/// 套接字保持阻塞模式：数据到达由内核即时唤醒线程，不再有 sleep 轮询地板。
/// 读超时只是停机标志的检查节拍（取消延迟上界 = `CANCELLABLE_READ_TIMEOUT`），
/// 写超时设长，避免慢客户端下半包被超时打断丢数据。
pub fn serve_cancellable(stream: TcpStream, stopping: Arc<AtomicBool>, runtime: Option<Arc<MysqlSqlRuntime>>) -> io::Result<()> {
    if !stream.peer_addr()?.ip().is_loopback() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "MySQL requires loopback until authentication and TLS are implemented"));
    }
    // 同 `serve_connection`：结果集的小包必须立即发出，否则延迟 ACK 会带来
    // 约 40ms 的往返地板，把取消检查节拍之外的延迟也一并抬高。
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CANCELLABLE_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CANCELLABLE_WRITE_TIMEOUT))?;
    serve_session(CancellableSocket { stream, stopping }, runtime)
}

fn serve_session(stream: impl Read + Write, runtime: Option<Arc<MysqlSqlRuntime>>) -> io::Result<()> {
    let mut reader = PacketReader::new(stream);
    let mut session = TransactionSession::new();
    let mut statements = MysqlPreparedRegistry::new();
    let mut result_buffer = Vec::new();
    write_packet(&mut reader, 0, &handshake())?;
    let response = match reader.read_packet()? {
        ReadPacket::Packet(1, response) => response,
        ReadPacket::Packet(_, _) => return Err(invalid_data("invalid MySQL handshake sequence")),
        ReadPacket::TooLarge(sequence) => {
            write_packet_too_large(&mut reader, sequence)?;
            return Ok(());
        }
        ReadPacket::Closed => {
            let _ = session.restore_schema(runtime.as_ref());
            return Ok(());
        }
    };
    let (username, empty_password, database) = match parse_handshake_response(&response) {
        Ok(parsed) => parsed,
        Err(error) => {
            write_error(&mut reader, 2, 1043, "08S01", error)?;
            return Ok(());
        }
    };
    if username.is_empty() || !empty_password {
        write_error(&mut reader, 2, 1045, "28000", "access denied: loopback accepts only an empty password")?;
        return Ok(());
    }
    session.set_schema(database.unwrap_or_default());
    write_ok(&mut reader, 2, session.status_flags())?;

    loop {
        let packet = reader.read_packet()?;
        let (sequence, payload) = match packet {
            ReadPacket::Packet(sequence, payload) => (sequence, payload),
            ReadPacket::TooLarge(sequence) => {
                write_packet_too_large(&mut reader, sequence)?;
                session.abandon(runtime.as_ref());
                return Ok(());
            }
            ReadPacket::Closed => {
                // A connection that goes away mid-transaction is an implicit rollback.
                session.abandon(runtime.as_ref());
                return Ok(());
            }
        };
        if sequence != 0 {
            write_error(&mut reader, sequence.wrapping_add(1), 1047, "08S01", "invalid command sequence")?;
            continue;
        }
        if payload.is_empty() {
            write_error(&mut reader, sequence.wrapping_add(1), 1047, "08S01", "empty command")?;
            continue;
        }
        match payload[0] {
            COM_QUIT => {
                session.abandon(runtime.as_ref());
                return Ok(());
            }
            // Drivers issue COM_PING to prove liveness before creating a cursor
            // and COM_INIT_DB when switching schema. The gateway keeps a single
            // catalog, so `COM_INIT_DB` only records the new default schema and
            // must not reset any session state, while `COM_PING` is a no-op.
            // Both are answered with the protocol OK packet.
            COM_PING => write_ok(&mut reader, sequence.wrapping_add(1), session.status_flags())?,
            COM_INIT_DB => match std::str::from_utf8(&payload[1..]) {
                Ok(schema) => {
                    session.set_schema(schema.to_string());
                    write_ok(&mut reader, sequence.wrapping_add(1), session.status_flags())?;
                }
                Err(_) => write_error(&mut reader, sequence.wrapping_add(1), 1049, "42000", "database name is not valid UTF-8")?,
            },
            // Connection pools (mysql-connector, SQLAlchemy `reset`) use this to
            // return an idle connection to its initial state. The socket and the
            // authentication stay untouched; only session state is reset.
            COM_RESET_CONNECTION => {
                session.reset_connection(runtime.as_ref())?;
                statements.clear();
                write_ok(&mut reader, sequence.wrapping_add(1), session.status_flags())?;
            }
            COM_STMT_PREPARE => handle_prepare(
                &mut reader,
                sequence.wrapping_add(1),
                &payload,
                runtime.as_ref(),
                &mut statements,
                &mut session,
                &mut result_buffer,
            )?,
            COM_STMT_EXECUTE => handle_execute(
                &mut reader,
                sequence.wrapping_add(1),
                &payload,
                runtime.as_ref(),
                &mut statements,
                &mut session,
                &mut result_buffer,
            )?,
            COM_STMT_CLOSE => {
                if let Some(id) = take_u32(&payload[1..]) {
                    statements.remove(&id);
                }
            }
            COM_STMT_RESET => handle_reset(&mut reader, sequence.wrapping_add(1), &payload, &statements, session.status_flags())?,
            COM_QUERY => {
                let sql = match std::str::from_utf8(&payload[1..]) {
                    Ok(sql) => sql,
                    Err(_) => {
                        write_error(&mut reader, sequence.wrapping_add(1), 1300, "HY000", "query is not valid UTF-8")?;
                        continue;
                    }
                };
                // The gateway owns packet framing and the response sequence; the
                // execution pipeline owns which response this statement gets and
                // every session transition behind it.
                let mut sink = WireSink { stream: &mut reader, sequence: sequence.wrapping_add(1), buffer: &mut result_buffer };
                dispatch_query(&mut sink, sql, runtime.as_ref(), &mut session)?;
            }
            // Every other command byte is either a real MySQL command this
            // gateway does not implement (`COM_CHANGE_USER`, `COM_STMT_FETCH`,
            // `COM_BINLOG_DUMP`, ...) or not a command at all. Answering with
            // `ER_UNKNOWN_COM_ERROR` keeps one request in, one response out: the
            // next packet still starts a new command, so the stream never shifts.
            command => {
                write_error(&mut reader, sequence.wrapping_add(1), ER_UNKNOWN_COM_ERROR, "HY000", &format!("Unknown command {command:#04X}"))?
            }
        }
    }
}

fn handshake() -> Vec<u8> {
    let mut payload = Vec::with_capacity(80);
    payload.push(PROTOCOL_VERSION);
    payload.extend_from_slice(b"8.0.36-yyds\0");
    payload.extend_from_slice(&1_u32.to_le_bytes());
    payload.extend_from_slice(b"yydsseed");
    payload.push(0);
    payload.extend_from_slice(&(SERVER_CAPABILITIES as u16).to_le_bytes());
    payload.push(45);
    payload.extend_from_slice(&2_u16.to_le_bytes());
    payload.extend_from_slice(&((SERVER_CAPABILITIES >> 16) as u16).to_le_bytes());
    payload.push(21);
    payload.extend_from_slice(&[0; 10]);
    payload.extend_from_slice(b"yydsseed1234");
    payload.push(0);
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

fn parse_handshake_response(payload: &[u8]) -> Result<(String, bool, Option<String>), &'static str> {
    if payload.len() < 32 {
        return Err("truncated handshake response");
    }
    let capabilities = u32::from_le_bytes(payload[..4].try_into().map_err(|_| "invalid capabilities")?);
    if capabilities & CLIENT_PROTOCOL_41 == 0 || capabilities & CLIENT_SSL != 0 {
        return Err("protocol 4.1 is required and TLS is unavailable");
    }
    let mut cursor = 32;
    let username = take_nul_bytes(payload, &mut cursor)?;
    let auth_response = if capabilities & CLIENT_PLUGIN_AUTH_LENENC != 0 {
        let auth_length = take_lenenc(payload, &mut cursor)?;
        let start = cursor;
        advance(payload, &mut cursor, auth_length)?;
        &payload[start..cursor]
    }
    else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let auth_length = usize::from(*payload.get(cursor).ok_or("missing auth response length")?);
        cursor += 1;
        let start = cursor;
        advance(payload, &mut cursor, auth_length)?;
        &payload[start..cursor]
    }
    else {
        take_nul_bytes(payload, &mut cursor)?
    };
    let mut database = None;
    if capabilities & CLIENT_CONNECT_WITH_DB != 0 {
        let value = take_nul_bytes(payload, &mut cursor)?;
        let value = String::from_utf8(value.to_vec()).map_err(|_| "database is not UTF-8")?;
        if !value.is_empty() {
            database = Some(value);
        }
    }
    if capabilities & CLIENT_PLUGIN_AUTH != 0 && cursor < payload.len() {
        let _ = take_nul_bytes(payload, &mut cursor)?;
    }
    let username = String::from_utf8(username.to_vec()).map_err(|_| "username is not UTF-8")?;
    Ok((username, auth_response.is_empty(), database))
}

fn take_nul_bytes<'a>(payload: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], &'static str> {
    let rest = payload.get(*cursor..).ok_or("truncated handshake response")?;
    let length = rest.iter().position(|byte| *byte == 0).ok_or("unterminated handshake field")?;
    let value = &rest[..length];
    *cursor += length + 1;
    Ok(value)
}

fn take_lenenc(payload: &[u8], cursor: &mut usize) -> Result<usize, &'static str> {
    match read_length_encoded(payload, cursor) {
        Ok(Some(value)) => usize::try_from(value).map_err(|_| "length-encoded integer overflow"),
        Ok(None) => Err("NULL auth response length is invalid"),
        Err(LenencError::Truncated) => Err("truncated length-encoded integer"),
        Err(LenencError::InvalidMarker) => Err("invalid auth response length"),
    }
}

fn advance(payload: &[u8], cursor: &mut usize, length: usize) -> Result<(), &'static str> {
    if length > MAX_PACKET || payload.get(*cursor..*cursor + length).is_none() {
        return Err("truncated or oversized auth response");
    }
    *cursor += length;
    Ok(())
}

/// Buffered classic-protocol reader.
///
/// One `read` returns whatever the OS has, which used to mean a separate
/// syscall for the 4-byte header and another for the payload of every command.
/// Keeping a session buffer lets one syscall serve a header plus its payload,
/// and pipelines the next command when the client already sent it.
struct PacketReader<R> {
    stream: R,
    buffer: Vec<u8>,
    start: usize,
}

impl<R: Read> PacketReader<R> {
    fn new(stream: R) -> Self {
        Self { stream, buffer: Vec::with_capacity(16 * 1024), start: 0 }
    }

    /// Ensures at least `needed` unconsumed bytes are buffered.
    /// Returns `false` when the peer closed before that many bytes arrived.
    fn fill(&mut self, needed: usize) -> io::Result<bool> {
        if self.buffer.len().saturating_sub(self.start) >= needed {
            return Ok(true);
        }
        if self.start > 0 {
            self.buffer.copy_within(self.start.., 0);
            let live = self.buffer.len() - self.start;
            self.buffer.truncate(live);
            self.start = 0;
        }
        while self.buffer.len() < needed {
            let want = (needed - self.buffer.len()).min(256 * 1024);
            let filled = self.buffer.len();
            self.buffer.resize(filled + want, 0);
            match self.stream.read(&mut self.buffer[filled..]) {
                Ok(0) => {
                    self.buffer.truncate(filled);
                    return Ok(false);
                }
                Ok(count) => self.buffer.truncate(filled + count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => self.buffer.truncate(filled),
                Err(error) => {
                    self.buffer.truncate(filled);
                    return Err(error);
                }
            }
        }
        Ok(true)
    }

    fn read_packet(&mut self) -> io::Result<ReadPacket> {
        if !self.fill(4)? {
            return if self.buffer.is_empty() { Ok(ReadPacket::Closed) } else { Err(invalid_data("truncated MySQL packet header")) };
        }
        let header = &self.buffer[self.start..self.start + 4];
        let length = usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let sequence = header[3];
        if length > MAX_PACKET {
            return Ok(ReadPacket::TooLarge(sequence));
        }
        if !self.fill(4 + length)? {
            return Err(invalid_data("truncated MySQL packet payload"));
        }
        let payload = self.buffer[self.start + 4..self.start + 4 + length].to_vec();
        self.start += 4 + length;
        Ok(ReadPacket::Packet(sequence, payload))
    }
}

impl<R: Write> Write for PacketReader<R> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Result of one wire frame read.
///
/// `TooLarge` is separated from I/O errors so the session can answer with
/// MySQL `1153` before closing instead of dropping the connection silently.
enum ReadPacket {
    Packet(u8, Vec<u8>),
    TooLarge(u8),
    Closed,
}

fn write_packet_too_large(stream: &mut impl Write, sequence: u8) -> io::Result<()> {
    write_error(stream, sequence.wrapping_add(1), 1153, "08S01", "Got a packet bigger than 'max_allowed_packet' bytes (1048576)")
}

fn write_packet(stream: &mut impl Write, sequence: u8, payload: &[u8]) -> io::Result<()> {
    let packet = encode_packet(sequence, payload).map_err(packet_error)?;
    stream.write_all(&packet)
}

fn write_ok(stream: &mut impl Write, sequence: u8, status_flags: u16) -> io::Result<()> {
    // OK identifier + affected_rows (length-encoded 0) + last_insert_id (length-encoded 0) + status_flags + warnings
    write_packet(stream, sequence, &ok_payload(0, 0, status_flags))
}

fn write_error(stream: &mut impl Write, sequence: u8, code: u16, state: &str, message: &str) -> io::Result<()> {
    write_packet(stream, sequence, &error_payload(code, state, message))
}

/// The gateway's [`MysqlStatementSink`] over one wire connection.
///
/// Execution decides the response and the session transitions behind it; this
/// adapter fixes the command's response sequence and performs packet framing.
struct WireSink<'a, R> {
    stream: &'a mut R,
    sequence: u8,
    buffer: &'a mut Vec<u8>,
}

impl<R: Write> MysqlStatementSink for WireSink<'_, R> {
    fn write_ok(&mut self, affected_rows: u64, last_insert_id: Option<u64>, status_flags: u16) -> io::Result<()> {
        write_packet(self.stream, self.sequence, &ok_payload(affected_rows, last_insert_id.unwrap_or(0), status_flags))
    }

    fn write_error(&mut self, code: u16, state: &str, message: &str) -> io::Result<()> {
        write_packet(self.stream, self.sequence, &error_payload(code, state, message))
    }

    fn write_result_set(
        &mut self,
        result: &SqlResult,
        column_types: &[u8],
        column_nullables: &[bool],
        binary_rows: bool,
        prefix: Option<(&[u8], usize)>,
        status_flags: u16,
    ) -> io::Result<()> {
        write_result_set(self.stream, self.sequence, result, column_types, column_nullables, binary_rows, prefix, status_flags, self.buffer)
    }
}

/// `COM_STMT_PREPARE`: validates names against the catalog, derives the result
/// shape and returns the statement id with parameter and column definitions.
fn handle_prepare(
    stream: &mut impl Write,
    sequence: u8,
    payload: &[u8],
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    statements: &mut MysqlPreparedRegistry,
    session: &mut TransactionSession,
    buffer: &mut Vec<u8>,
) -> io::Result<()> {
    let sql = match std::str::from_utf8(&payload[1..]) {
        Ok(sql) => sql,
        Err(_) => {
            write_error(stream, sequence, 1300, "HY000", "query is not valid UTF-8")?;
            return Ok(());
        }
    };
    let outcome = {
        let mut sink = WireSink { stream, sequence, buffer };
        prepare_statement(&mut sink, sql, runtime, session)?
    };
    let MysqlPrepareOutcome::Statement { parameter_count, columns } = outcome
    else {
        return Ok(());
    };
    let id = statements.allocate();
    write_prepare_ok(stream, sequence, id, parameter_count as u16, columns.len() as u16)?;
    let mut next = sequence.wrapping_add(1);
    if parameter_count > 0 {
        for _ in 0..parameter_count {
            write_packet(stream, next, &encode_column_definition("?", MYSQL_TYPE_VAR_STRING, false))?;
            next = next.wrapping_add(1);
        }
        write_eof(stream, next, session.status_flags())?;
        next = next.wrapping_add(1);
    }
    if !columns.is_empty() {
        for column in &columns {
            write_packet(stream, next, &encode_column_definition(&column.name, column.type_code, column.nullable))?;
            next = next.wrapping_add(1);
        }
        write_eof(stream, next, session.status_flags())?;
    }
    statements.insert(
        id,
        MysqlPreparedStatement::new(
            sql.to_string(),
            parameter_count,
            columns.iter().map(|column| column.type_code).collect(),
            columns.iter().map(|column| column.nullable).collect(),
        ),
    );
    Ok(())
}

fn write_prepare_ok(stream: &mut impl Write, sequence: u8, statement_id: u32, parameter_count: u16, column_count: u16) -> io::Result<()> {
    write_packet(stream, sequence, &prepare_ok_payload(statement_id, parameter_count, column_count))
}

/// `COM_STMT_EXECUTE`: decodes the typed parameter block, renders the statement
/// with real values and runs it through the shared SQL pipeline.
fn handle_execute(
    stream: &mut impl Write,
    sequence: u8,
    payload: &[u8],
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    statements: &mut MysqlPreparedRegistry,
    session: &mut TransactionSession,
    buffer: &mut Vec<u8>,
) -> io::Result<()> {
    let Some(statement_id) = take_u32(&payload[1..])
    else {
        write_error(stream, sequence, 1210, "HY000", "malformed COM_STMT_EXECUTE command")?;
        return Ok(());
    };
    // statement id (4) + flags (1) + iteration count (4)
    let mut cursor = 5;
    let Some(_flags) = payload.get(cursor)
    else {
        write_error(stream, sequence, 1210, "HY000", "malformed COM_STMT_EXECUTE command")?;
        return Ok(());
    };
    cursor += 1;
    let Some(iteration) = take_u32(&payload[cursor..])
    else {
        write_error(stream, sequence, 1210, "HY000", "malformed COM_STMT_EXECUTE iteration count")?;
        return Ok(());
    };
    cursor += 4;
    if iteration != 1 {
        write_error(stream, sequence, 1210, "HY000", "COM_STMT_EXECUTE iteration count must be 1")?;
        return Ok(());
    }
    let Some(statement) = statements.get_mut(&statement_id)
    else {
        write_error(stream, sequence, 1243, "HY000", &format!("Unknown prepared statement handler ({statement_id})"))?;
        return Ok(());
    };

    let mut values = Vec::with_capacity(statement.parameter_count);
    if statement.parameter_count > 0 {
        let bitmap_length = statement.parameter_count.div_ceil(8);
        let Some(bitmap) = payload.get(cursor..cursor + bitmap_length)
        else {
            write_error(stream, sequence, 1210, "HY000", "truncated COM_STMT_EXECUTE null bitmap")?;
            return Ok(());
        };
        cursor += bitmap_length;
        let Some(&bound_flag) = payload.get(cursor)
        else {
            write_error(stream, sequence, 1210, "HY000", "missing COM_STMT_EXECUTE parameter types")?;
            return Ok(());
        };
        cursor += 1;
        if bound_flag == 1 {
            let mut types = Vec::with_capacity(statement.parameter_count);
            for _ in 0..statement.parameter_count {
                let Some(&code) = payload.get(cursor)
                else {
                    write_error(stream, sequence, 1210, "HY000", "truncated COM_STMT_EXECUTE parameter types")?;
                    return Ok(());
                };
                let Some(&flags) = payload.get(cursor + 1)
                else {
                    write_error(stream, sequence, 1210, "HY000", "truncated COM_STMT_EXECUTE parameter types")?;
                    return Ok(());
                };
                types.push((code, flags & 0x80 != 0));
                cursor += 2;
            }
            statement.parameter_types = types;
        }
        else if statement.parameter_types.len() != statement.parameter_count {
            write_error(stream, sequence, 1210, "HY000", "COM_STMT_EXECUTE reused parameter types before they were bound")?;
            return Ok(());
        }
        for index in 0..statement.parameter_count {
            if bitmap[index / 8] & (1 << (index % 8)) != 0 {
                values.push(SqlValue::Null);
                continue;
            }
            let (code, unsigned) = statement.parameter_types[index];
            match decode_parameter(payload, &mut cursor, code, unsigned) {
                Ok(value) => values.push(value),
                Err(message) => {
                    write_error(stream, sequence, 1235, "42000", &message)?;
                    return Ok(());
                }
            }
        }
    }
    let mut sink = WireSink { stream, sequence, buffer };
    execute_prepared(&mut sink, statement_id, &values, runtime, statements, session)
}

/// `COM_STMT_RESET` clears the cached parameter binding and returns OK.
fn handle_reset(
    stream: &mut impl Write,
    sequence: u8,
    payload: &[u8],
    statements: &MysqlPreparedRegistry,
    status_flags: u16,
) -> io::Result<()> {
    let Some(statement_id) = take_u32(&payload[1..])
    else {
        return write_error(stream, sequence, 1210, "HY000", "malformed COM_STMT_RESET command");
    };
    if statements.contains(&statement_id) {
        write_ok(stream, sequence, status_flags)
    }
    else {
        write_error(stream, sequence, 1243, "HY000", &format!("Unknown prepared statement handler ({statement_id})"))
    }
}

/// Decodes one `COM_STMT_EXECUTE` parameter value by its `MYSQL_TYPE_*` code.
fn decode_parameter(payload: &[u8], cursor: &mut usize, code: u8, unsigned: bool) -> Result<SqlValue, String> {
    match code {
        MYSQL_TYPE_TINY => {
            let bytes = take_slice(payload, cursor, 1)?;
            Ok(SqlValue::Integer(if unsigned { i64::from(bytes[0]) } else { i64::from(bytes[0] as i8) }))
        }
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => {
            let bytes = take_slice(payload, cursor, 2)?;
            let raw = u16::from_le_bytes([bytes[0], bytes[1]]);
            Ok(SqlValue::Integer(if unsigned { i64::from(raw) } else { i64::from(raw as i16) }))
        }
        MYSQL_TYPE_INT24 => {
            let bytes = take_slice(payload, cursor, 3)?;
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
            Ok(SqlValue::Integer(if unsigned { i64::from(raw) } else { i64::from(((raw << 8) as i32) >> 8) }))
        }
        MYSQL_TYPE_LONG => {
            let bytes = take_slice(payload, cursor, 4)?;
            let raw = u32::from_le_bytes(bytes.try_into().expect("checked length"));
            Ok(SqlValue::Integer(if unsigned { i64::from(raw) } else { i64::from(raw as i32) }))
        }
        MYSQL_TYPE_LONGLONG => {
            let bytes = take_slice(payload, cursor, 8)?;
            let raw = u64::from_le_bytes(bytes.try_into().expect("checked length"));
            Ok(SqlValue::Integer(raw as i64))
        }
        MYSQL_TYPE_FLOAT => {
            let bytes = take_slice(payload, cursor, 4)?;
            Ok(SqlValue::Number(f64::from(f32::from_le_bytes(bytes.try_into().expect("checked length")))))
        }
        MYSQL_TYPE_DOUBLE => {
            let bytes = take_slice(payload, cursor, 8)?;
            Ok(SqlValue::Number(f64::from_le_bytes(bytes.try_into().expect("checked length"))))
        }
        MYSQL_TYPE_NULL => Ok(SqlValue::Null),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
            let length = take_length_encoded(payload, cursor)?;
            let bytes = take_slice(payload, cursor, length)?;
            let text = std::str::from_utf8(bytes).map_err(|_| format!("COM_STMT_EXECUTE parameter type {code:#04X} is not valid UTF-8"))?;
            Ok(SqlValue::String(text.to_string()))
        }
        // BLOB 家族在二进制协议里就是原始 octets，原样收下，不做文本解码。
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            let length = take_length_encoded(payload, cursor)?;
            Ok(SqlValue::Bytes(take_slice(payload, cursor, length)?.to_vec()))
        }
        _ => Err(format!("COM_STMT_EXECUTE parameter type {code:#04X} is not supported")),
    }
}

fn take_slice<'a>(payload: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], String> {
    let end = cursor.checked_add(length).ok_or_else(|| "truncated COM_STMT_EXECUTE parameter".to_string())?;
    let bytes = payload.get(*cursor..end).ok_or_else(|| "truncated COM_STMT_EXECUTE parameter".to_string())?;
    *cursor = end;
    Ok(bytes)
}

fn take_length_encoded(payload: &[u8], cursor: &mut usize) -> Result<usize, String> {
    match read_length_encoded(payload, cursor) {
        Ok(Some(value)) => usize::try_from(value).map_err(|_| "length-encoded parameter length exceeds the address space".to_string()),
        Ok(None) => Err("NULL length-encoded parameter must be flagged in the null bitmap".to_string()),
        Err(LenencError::Truncated) => Err("truncated length-encoded parameter".to_string()),
        Err(LenencError::InvalidMarker) => Err("invalid length-encoded parameter length".to_string()),
    }
}

fn take_u32(bytes: &[u8]) -> Option<u32> {
    bytes.get(..4).map(|slice| u32::from_le_bytes(slice.try_into().expect("checked length")))
}

/// 把 SqlResult 编码成 MySQL ResultSet 协议包。
///
/// 结果集的每个包都带自己的序号，但读取端按 TCP 流解析，不要求一包一次
/// `write`。这里把列定义、行与结尾 EOF 追加进同一个缓冲区，攒满
/// [`RESULT_FLUSH_BYTES`] 再写出：1000 行结果从 1000+ 次小 `write` 降为个位数次，
/// 帧头与载荷只复制一次，也不再为每行单独分配 `Vec`。
fn write_result_set(
    stream: &mut impl Write,
    mut sequence: u8,
    result: &SqlResult,
    column_types: &[u8],
    column_nullables: &[bool],
    binary_rows: bool,
    prefix: Option<(&[u8], usize)>,
    status_flags: u16,
    buffer: &mut Vec<u8>,
) -> io::Result<()> {
    // 会话级缓冲区跨语句复用：首次按 8 KiB 起步，之后容量随最大结果集增长一次，
    // 单行结果不再每条语句重新分配一个暂存向量。
    buffer.clear();
    if buffer.capacity() == 0 {
        buffer.reserve(RESULT_FLUSH_BYTES.min(8 * 1024));
    }

    // 缓存计划的热路径直接复用已经编码好的列定义；序号从 1 开始时这些
    // 帧头才与调用方的序号一致，其它序号回退到逐列编码。
    match prefix.filter(|_| sequence == 1) {
        Some((bytes, count)) if !bytes.is_empty() => {
            buffer.extend_from_slice(bytes);
            // The classic protocol wraps the sequence byte at 255, and a result
            // set may carry more than 253 columns.
            sequence = sequence.wrapping_add(1u8.wrapping_add(count as u8));
        }
        _ => {
            // 包 1: column count
            let count = result.columns.len();
            let mut col_count = Vec::new();
            encode_length_encoded(&mut col_count, count as u64);
            push_packet(buffer, sequence, &col_count)?;
            sequence = sequence.wrapping_add(1);

            // 包 2..N: column definitions
            for (index, col) in result.columns.iter().enumerate() {
                let declared = column_types.get(index).copied();
                let nullable = column_nullables.get(index).copied().unwrap_or(true);
                let def = encode_column_definition(col, declared.unwrap_or_else(|| column_type(&result.rows, index)), nullable);
                push_packet(buffer, sequence, &def)?;
                sequence = sequence.wrapping_add(1);
            }
        }
    }

    // EOF after columns
    push_eof(buffer, sequence, status_flags)?;
    sequence = sequence.wrapping_add(1);

    // Row packets
    for row in &result.rows {
        if binary_rows {
            let payload = encode_binary_row(row, column_types);
            push_packet(buffer, sequence, &payload)?;
        }
        else {
            push_text_row(buffer, sequence, row)?;
        }
        sequence = sequence.wrapping_add(1);
        if buffer.len() >= RESULT_FLUSH_BYTES {
            stream.write_all(buffer)?;
            buffer.clear();
        }
    }

    // EOF after rows
    push_eof(buffer, sequence, status_flags)?;
    stream.write_all(buffer)
}

/// 结果集写出的攒批阈值：大于一次 MTU，小于 `MAX_PACKET`，兼顾延迟与系统调用次数。
const RESULT_FLUSH_BYTES: usize = 64 * 1024;

/// 把一条已经编码好的载荷按 classic 协议组帧追加到缓冲区。
fn push_packet(out: &mut Vec<u8>, sequence: u8, payload: &[u8]) -> io::Result<()> {
    append_packet(out, sequence, payload).map_err(packet_error)
}

/// 把 EOF 包追加到结果集缓冲区。
fn push_eof(out: &mut Vec<u8>, sequence: u8, status_flags: u16) -> io::Result<()> {
    push_packet(out, sequence, &eof_payload(status_flags))
}

/// 直接以 4 字节帧头占位写一行文本行，载荷长度算完后回填，省掉每行的临时 `Vec`。
fn push_text_row(out: &mut Vec<u8>, sequence: u8, row: &[SqlValue]) -> io::Result<()> {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, sequence]);
    for value in row {
        encode_sql_value(out, value);
    }
    let length = out.len() - start - 4;
    if length > MAX_PACKET_PAYLOAD {
        out.truncate(start);
        return Err(packet_error(PacketError::PayloadTooLarge));
    }
    out[start] = length as u8;
    out[start + 1] = (length >> 8) as u8;
    out[start + 2] = (length >> 16) as u8;
    Ok(())
}

/// 按列声明类型把一行编码成 MySQL binary protocol row（prepared statement 结果）。
fn encode_binary_row(row: &[SqlValue], column_types: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x00); // binary row header
    let mut bitmap = vec![0_u8; (row.len() + 2).div_ceil(8)];
    for (index, value) in row.iter().enumerate() {
        if matches!(value, SqlValue::Null) {
            let bit = index + 2;
            bitmap[bit / 8] |= 1 << (bit % 8);
        }
    }
    out.extend_from_slice(&bitmap);
    for (index, value) in row.iter().enumerate() {
        if matches!(value, SqlValue::Null) {
            continue;
        }
        let column_type = column_types.get(index).copied().unwrap_or(MYSQL_TYPE_VAR_STRING);
        match (column_type, value) {
            (MYSQL_TYPE_LONGLONG, SqlValue::Integer(number)) => out.extend_from_slice(&number.to_le_bytes()),
            (MYSQL_TYPE_LONG, SqlValue::Integer(number)) => out.extend_from_slice(&(*number as i32).to_le_bytes()),
            (MYSQL_TYPE_INT24, SqlValue::Integer(number)) => {
                out.extend_from_slice(&(*number as i32).to_le_bytes()[..3]);
            }
            (MYSQL_TYPE_SHORT, SqlValue::Integer(number)) => out.extend_from_slice(&(*number as i16).to_le_bytes()),
            (MYSQL_TYPE_TINY, SqlValue::Integer(number)) => out.push(*number as u8),
            (MYSQL_TYPE_TINY, SqlValue::Boolean(truth)) => out.push(u8::from(*truth)),
            (MYSQL_TYPE_DOUBLE, SqlValue::Number(number)) => out.extend_from_slice(&number.to_le_bytes()),
            (MYSQL_TYPE_FLOAT, SqlValue::Number(number)) => out.extend_from_slice(&(*number as f32).to_le_bytes()),
            (MYSQL_TYPE_DATE, SqlValue::String(text)) => encode_binary_date(&mut out, text, false),
            (MYSQL_TYPE_DATETIME, SqlValue::String(text)) => encode_binary_date(&mut out, text, true),
            _ => encode_sql_value(&mut out, value),
        }
    }
    out
}

/// `DATE`/`DATETIME` 的 binary protocol 编码：长度字节 + 年（u16 LE）+ 月 + 日，
/// `DATETIME` 再追加时/分/秒（缺省补 0，长度恒为 7），与真库报文布局一致，
/// mysql2 据此还原 JS Date。无法按日期文本解析的值退回文本编码，不伪造字段。
fn encode_binary_date(out: &mut Vec<u8>, text: &str, with_time: bool) {
    let parsed: Option<Vec<u8>> = (|| {
        let (date, time) = text.split_once(' ').unwrap_or((text, ""));
        let mut parts = date.split('-');
        let year = parts.next()?.parse::<u16>().ok()?;
        let month = parts.next()?.parse::<u8>().ok()?;
        let day = parts.next()?.parse::<u8>().ok()?;
        let mut fields = Vec::with_capacity(7);
        fields.extend_from_slice(&year.to_le_bytes());
        fields.push(month);
        fields.push(day);
        if with_time {
            let mut parts = time.split(':');
            fields.push(parts.next().and_then(|value| value.parse::<u8>().ok()).unwrap_or(0));
            fields.push(parts.next().and_then(|value| value.parse::<u8>().ok()).unwrap_or(0));
            fields.push(parts.next().and_then(|value| value.parse::<u8>().ok()).unwrap_or(0));
        }
        Some(fields)
    })();
    match parsed {
        Some(fields) => {
            out.push(fields.len() as u8);
            out.extend_from_slice(&fields);
        }
        None => encode_sql_value(out, &SqlValue::String(text.to_string())),
    }
}

/// 按首批非空值推断 MySQL 列类型：`LONGLONG`、`DOUBLE`、`TINY`、`VAR_STRING`。
fn column_type(rows: &[Vec<SqlValue>], index: usize) -> u8 {
    rows.iter()
        .find_map(|row| match row.get(index) {
            Some(SqlValue::Integer(_)) => Some(MYSQL_TYPE_LONGLONG),
            Some(SqlValue::Number(_)) => Some(MYSQL_TYPE_DOUBLE),
            Some(SqlValue::Boolean(_)) => Some(MYSQL_TYPE_TINY),
            Some(SqlValue::String(_)) => Some(MYSQL_TYPE_VAR_STRING),
            Some(SqlValue::Bytes(_)) => Some(MYSQL_TYPE_BLOB),
            // 数组只出现在 PostgreSQL 面；MySQL 面兜底按文本上报。
            Some(SqlValue::Array(_)) => Some(MYSQL_TYPE_VAR_STRING),
            Some(SqlValue::Null) | None => None,
        })
        .unwrap_or(MYSQL_TYPE_VAR_STRING)
}

/// MySQL wire protocol ColumnDefinition 编码，委托 protocol.mysql 的 payload 原语。
fn encode_column_definition(name: &str, column_type: u8, nullable: bool) -> Vec<u8> {
    column_definition_payload(name, column_type, nullable, MYSQL_BLOB_CAPACITY as u32)
}

fn encode_sql_value(out: &mut Vec<u8>, value: &SqlValue) {
    match value {
        SqlValue::Null => out.push(0xFB),
        SqlValue::Integer(n) => encode_integer_text(out, *n),
        SqlValue::Number(n) => {
            let text = format!("{n}");
            encode_length_encoded(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        SqlValue::String(s) => {
            encode_length_encoded(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        // 原始 octets 按 length-encoded string 原样发出，文本与二进制行协议一致。
        SqlValue::Bytes(bytes) => {
            encode_length_encoded(out, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
        // 数组只由 PostgreSQL 面产出；MySQL 面用 PostgreSQL 文本数组字面量兜底。
        SqlValue::Array(values) => {
            let text = format!("{{{}}}", values.iter().map(|value| format!("{value:?}")).collect::<Vec<_>>().join(","));
            encode_length_encoded(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        SqlValue::Boolean(b) => {
            let text = if *b { "1" } else { "0" };
            encode_length_encoded(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
    }
}

/// 把整数按十进制写进缓冲区。
///
/// 文本行协议要求十进制字符串，但每个单元格都走 `i64::to_string` 会为整数列
/// 每行分配一次堆，扫描 1000 行就是上千次短命分配。这里改用栈上 20 字节暂存。
fn encode_integer_text(out: &mut Vec<u8>, value: i64) {
    let mut digits = [0_u8; 20];
    let mut position = digits.len();
    let negative = value < 0;
    let mut remaining = value.unsigned_abs();
    loop {
        position -= 1;
        digits[position] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    if negative {
        position -= 1;
        digits[position] = b'-';
    }
    encode_length_encoded(out, (digits.len() - position) as u64);
    out.extend_from_slice(&digits[position..]);
}

fn write_eof(stream: &mut impl Write, sequence: u8, status_flags: u16) -> io::Result<()> {
    // 简化 EOF packet: 0xFE + 0x0000 (warnings) + status_flags
    write_packet(stream, sequence, &eof_payload(status_flags))
}

fn packet_error(error: PacketError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid MySQL packet: {error:?}"))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// 可取消会话的读超时节拍：阻塞读在数据到达时被内核即时唤醒；超时到期只
/// 意味着“这段时间没有数据”，回到循环顶检查停机标志后继续等。取消延迟上界即此值。
const CANCELLABLE_READ_TIMEOUT: Duration = Duration::from_millis(50);

/// 可取消会话的写超时：设长，避免慢客户端下发送半包时被超时打断丢数据。
const CANCELLABLE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 单次 I/O 操作的总时限，与 `serve_connection` 的 30 秒空闲超时口径一致。
const CANCELLABLE_IO_DEADLINE: Duration = Duration::from_secs(30);

struct CancellableSocket {
    stream: TcpStream,
    stopping: Arc<AtomicBool>,
}

impl CancellableSocket {
    fn attempt<T>(&mut self, mut operation: impl FnMut(&mut TcpStream) -> io::Result<T>) -> io::Result<T> {
        let deadline = Instant::now() + CANCELLABLE_IO_DEADLINE;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "MySQL service stopping"));
            }
            match operation(&mut self.stream) {
                // 阻塞套接字 + 短超时：超时到期仅代表“这段时间没有数据到达 / 缓冲未腾空”，
                // 检查停机标志后继续等。Linux 超时回 WouldBlock，Windows 回 TimedOut，两者都按可重试处理。
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "MySQL socket I/O timeout"));
                    }
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
