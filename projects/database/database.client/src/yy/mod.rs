//! Rust **remote** client for **`yydb serve`** / **`yyds serve`** (YY wire `YYDB`+`0000` over TCP or WebSocket).
//!
//! This is the non-embedded Rust surface. For in-process access use the
//! [`yydb`] embed facade. Browser and Node TypeScript hosts should use
//! `@yydb/yydb-client`. The all-in-one Node product is `@yydb/yydb` (CLI + engine + serve).
//!
//! The client only projects operations: table handles build VOS sources and
//! send them through the shared `Prepare` / `ExecutePrepared` frames, so the
//! server executes the same VOS programs as the embedded engine.
//!
//! YYDB serve has **no TLS and no ACL beyond bearer tokens**. Connect only to
//! endpoints you control.

#![deny(missing_docs)]

use std::{
    collections::BTreeMap,
    fmt, io,
    net::{Shutdown, TcpStream},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
};

pub use database_types::{Error, Result, SchemaVersion, Value};
pub use database_udf::HostMicroDefinition;

use database_protocol_yy::{
    ExecutePreparedRows, Frame, MessageType, decode_error, decode_execute_prepared_ok, decode_kv_get_ok, decode_named_rows, decode_prepare_ok,
    decode_rows, decode_scalar_call_ok, decode_schema_get_ok, encode_authenticate, encode_execute_prepared, encode_kv_get, encode_kv_put,
    encode_micro_register, encode_params, encode_prepare, encode_savepoint_name, encode_scalar_call, encode_schema_ensure,
    encode_session_close, read_frame as read_tcp_frame, split_bind, write_frame as write_tcp_frame,
};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// A result row keyed by column name.
pub type Row = BTreeMap<String, Value>;

/// Lifecycle state of a [`Client`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// The transport is open and requests can be sent.
    Connected,
    /// The transport was dropped with [`Client::disconnect`]; [`Client::reconnect`] can restore it.
    Disconnected,
    /// [`Client::close`] completed; the client is terminal and cannot reconnect.
    Closed,
}

/// Handle to a remote YY server started with `yydb serve` or `yyds serve`.
pub struct Client {
    endpoint: String,
    transport: TransportKind,
    state: Mutex<SessionState>,
    next_id: AtomicU32,
}

struct SessionState {
    link: Option<Link>,
    /// Bootstrap bearer token used for `Authenticate`; replayed on reconnect.
    credential: Option<String>,
    /// Opaque token returned by the last `AuthenticateOk`.
    session_token: Option<String>,
    closed: bool,
}

impl SessionState {
    fn state(&self) -> ClientState {
        if self.closed {
            ClientState::Closed
        }
        else if self.link.is_some() {
            ClientState::Connected
        }
        else {
            ClientState::Disconnected
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportKind {
    Tcp,
    WebSocket,
}

/// One established transport: a TCP stream plus the framing rule it speaks.
struct Link {
    stream: TcpStream,
    kind: TransportKind,
}

impl Link {
    fn connect(endpoint: &str, kind: TransportKind) -> Result<Self> {
        let mut stream = TcpStream::connect(endpoint)?;
        if kind == TransportKind::WebSocket {
            websocket_handshake(&mut stream, endpoint)?;
        }
        Ok(Self { stream, kind })
    }

    fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        match self.kind {
            TransportKind::Tcp => write_tcp_frame(&mut self.stream, frame),
            TransportKind::WebSocket => {
                let bytes = frame.encode()?;
                write_websocket_binary(&mut self.stream, &bytes)
            }
        }
    }

    fn read_frame(&mut self) -> Result<Frame> {
        match self.kind {
            TransportKind::Tcp => read_tcp_frame(&mut self.stream),
            TransportKind::WebSocket => {
                let payload = read_websocket_binary(&mut self.stream)?;
                Frame::decode(&payload)
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl Client {
    /// Connect to a serve endpoint such as `127.0.0.1:7700`, `tcp://127.0.0.1:7700`
    /// or `ws://127.0.0.1:7700`.
    ///
    /// Opens the transport and performs a `Hello` handshake. A `ws://` endpoint
    /// performs the WebSocket upgrade on `/wire` and then speaks the same YY
    /// frames; `wss://` is rejected because TLS is not part of this client.
    pub fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let (transport, endpoint) = normalize_endpoint(endpoint.into())?;
        let link = Link::connect(&endpoint, transport)?;
        let client = Self {
            endpoint,
            transport,
            state: Mutex::new(SessionState { link: Some(link), credential: None, session_token: None, closed: false }),
            next_id: AtomicU32::new(1),
        };
        if let Err(error) = client.hello() {
            let _ = client.close();
            return Err(error);
        }
        Ok(client)
    }

    /// Connect and authenticate with a bootstrap token in the same handshake.
    ///
    /// Equivalent to [`Self::connect`] followed by [`Self::authenticate`].
    pub fn connect_with_token(endpoint: impl Into<String>, token: &str) -> Result<Self> {
        let client = Self::connect(endpoint)?;
        if let Err(error) = client.authenticate(token) {
            let _ = client.close();
            return Err(error);
        }
        Ok(client)
    }

    /// Attach a session to this connection with a bootstrap token.
    ///
    /// The token is remembered so [`Self::reconnect`] can re-authenticate a
    /// replacement transport without the caller re-supplying credentials.
    pub fn authenticate(&self, token: &str) -> Result<()> {
        let response = self.round_trip(MessageType::Authenticate, encode_authenticate(token))?;
        expect_type(&response, MessageType::AuthenticateOk)?;
        let session_token = String::from_utf8_lossy(&response.body).into_owned();
        let mut state = self.lock();
        state.credential = Some(token.to_owned());
        state.session_token = Some(session_token);
        Ok(())
    }

    /// Configured endpoint string (`host:port`).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ClientState {
        self.lock().state()
    }

    /// Whether requests can currently be sent.
    pub fn is_connected(&self) -> bool {
        self.state() == ClientState::Connected
    }

    /// Server version from the latest hello (re-issues Hello).
    pub fn server_version(&self) -> Result<String> {
        self.hello()
    }

    /// Fetch remote schema metadata.
    pub fn schema(&self) -> Result<Option<SchemaVersion>> {
        let response = self.round_trip(MessageType::SchemaGet, Vec::new())?;
        expect_type(&response, MessageType::SchemaGetOk)?;
        decode_schema_get_ok(&response.body)
    }

    /// Ensure the remote schema (VOS document).
    ///
    /// A document that declares `table Item { … }` is the remote table creation
    /// primitive; the same schema is then visible to prepared statements and
    /// [`Self::table`] projections.
    pub fn ensure_schema(&self, document: &str) -> Result<()> {
        let response = self.round_trip(MessageType::SchemaEnsure, encode_schema_ensure(0, document))?;
        expect_type(&response, MessageType::SchemaEnsureOk)?;
        Ok(())
    }

    /// Fetch remote info text (`path=…\n…`).
    pub fn info(&self) -> Result<String> {
        let response = self.round_trip(MessageType::Info, Vec::new())?;
        expect_type(&response, MessageType::InfoOk)?;
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }

    /// Get a KV record.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let response = self.round_trip(MessageType::KvGet, encode_kv_get(key))?;
        expect_type(&response, MessageType::KvGetOk)?;
        decode_kv_get_ok(&response.body)
    }

    /// Put a KV record.
    pub fn put(&self, key: &str, value: impl AsRef<[u8]>) -> Result<()> {
        let response = self.round_trip(MessageType::KvPut, encode_kv_put(key, value.as_ref()))?;
        expect_type(&response, MessageType::KvPutOk)?;
        Ok(())
    }

    /// Register a session-local host micro contract on the remote engine.
    pub fn register_host_micro(&self, definition: &HostMicroDefinition) -> Result<()> {
        let response = self.round_trip(MessageType::MicroRegister, encode_micro_register(definition)?)?;
        expect_type(&response, MessageType::MicroRegisterOk)?;
        Ok(())
    }

    /// Invoke a registered scalar UDF on the remote engine.
    pub fn call_scalar_version(&self, name: &str, version: u32, args: &[Value]) -> Result<Value> {
        let response = self.round_trip(MessageType::ScalarCall, encode_scalar_call(name, version, args)?)?;
        expect_type(&response, MessageType::ScalarCallOk)?;
        decode_scalar_call_ok(&response.body)
    }

    /// Compile a VOS source into a reusable server-side plan.
    pub fn prepare(&self, source: &str) -> Result<String> {
        let response = self.round_trip(MessageType::Prepare, encode_prepare(source))?;
        expect_type(&response, MessageType::PrepareOk)?;
        decode_prepare_ok(&response.body)
    }

    /// Execute a prepared statement with named parameters.
    pub fn execute_prepared(&self, statement_id: &str, params: &BTreeMap<String, Value>) -> Result<Vec<Row>> {
        let encoded = if params.is_empty() { None } else { Some(encode_params(params)?) };
        let response = self.round_trip(MessageType::ExecutePrepared, encode_execute_prepared(statement_id, encoded.as_deref()))?;
        expect_type(&response, MessageType::ExecutePreparedOk)?;
        match decode_execute_prepared_ok(&response.body)? {
            ExecutePreparedRows::Plain(rows) => decode_rows(&rows),
            ExecutePreparedRows::Named { directory, rows } => decode_named_rows(&rows, &directory),
        }
    }

    /// Prepare and execute in one call.
    pub fn query(&self, source: &str, params: &BTreeMap<String, Value>) -> Result<Vec<Row>> {
        let statement_id = self.prepare(source)?;
        self.execute_prepared(&statement_id, params)
    }

    /// Open a client-side projection of one VOS table.
    ///
    /// The handle only builds VOS sources; execution stays on the server, so
    /// remote and embedded hosts run the same program.
    pub fn table(&self, name: &str) -> Result<Table<'_>> {
        validate_identifier(name)?;
        Ok(Table { client: self, name: name.to_owned() })
    }

    /// Open an explicit transaction on this connection.
    pub fn begin(&self) -> Result<()> {
        self.expect_ok(MessageType::TxnBegin, Vec::new(), MessageType::TxnBeginOk)
    }

    /// Commit the explicit transaction.
    pub fn commit(&self) -> Result<()> {
        self.expect_ok(MessageType::TxnCommit, Vec::new(), MessageType::TxnCommitOk)
    }

    /// Roll the explicit transaction back.
    pub fn rollback(&self) -> Result<()> {
        self.expect_ok(MessageType::TxnRollback, Vec::new(), MessageType::TxnRollbackOk)
    }

    /// Create a named savepoint inside the open transaction.
    pub fn savepoint(&self, name: &str) -> Result<()> {
        self.expect_ok(MessageType::TxnSavepoint, encode_savepoint_name(name), MessageType::TxnSavepointOk)
    }

    /// Roll back to a named savepoint without ending the transaction.
    pub fn rollback_to_savepoint(&self, name: &str) -> Result<()> {
        self.expect_ok(MessageType::TxnRollbackTo, encode_savepoint_name(name), MessageType::TxnRollbackToOk)
    }

    /// Release a named savepoint.
    pub fn release_savepoint(&self, name: &str) -> Result<()> {
        self.expect_ok(MessageType::TxnRelease, encode_savepoint_name(name), MessageType::TxnReleaseOk)
    }

    /// Fetch a runtime performance snapshot (JSON) from the remote node.
    pub fn perf_report(&self) -> Result<String> {
        let response = self.round_trip(MessageType::PerfReport, Vec::new())?;
        expect_type(&response, MessageType::PerfReportOk)?;
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }

    /// Drop the transport but keep the client reusable.
    ///
    /// A later business call fails with a not-connected protocol error until
    /// [`Self::reconnect`] establishes a fresh transport and replays the stored
    /// credential.
    pub fn disconnect(&self) -> Result<()> {
        let mut state = self.lock();
        if state.closed {
            return Err(Error::SessionClosed);
        }
        if let Some(mut link) = state.link.take() {
            link.shutdown();
        }
        Ok(())
    }

    /// Re-open a transport after [`Self::disconnect`] or a dropped socket.
    ///
    /// Re-runs `Hello` and, when a credential is stored, `Authenticate` on the
    /// new connection. Prepared statements are per-connection on the server, so
    /// callers must prepare again; result-unknown writes are never replayed.
    pub fn reconnect(&self) -> Result<()> {
        let endpoint = self.endpoint.clone();
        let transport = self.transport;
        let credential = {
            let mut state = self.lock();
            if state.closed {
                return Err(Error::SessionClosed);
            }
            state.link = None;
            state.credential.clone()
        };
        let link = Link::connect(&endpoint, transport)?;
        self.lock().link = Some(link);
        if let Err(error) = self.hello() {
            self.drop_link();
            return Err(error);
        }
        if let Some(token) = credential {
            if let Err(error) = self.authenticate(&token) {
                self.drop_link();
                return Err(error);
            }
        }
        Ok(())
    }

    /// Explicitly close the session and release its server-side lease.
    ///
    /// Sends `SessionClose` so the server drops prepared statements, rolls back
    /// an open transaction and releases the write lease before the socket is
    /// torn down. The client becomes terminal: later calls fail with
    /// [`Error::SessionClosed`] and [`Self::reconnect`] refuses to revive it.
    /// Repeated calls are successful.
    pub fn close(&self) -> Result<()> {
        let mut state = self.lock();
        if state.closed {
            return Ok(());
        }
        if let Some(link) = state.link.as_mut() {
            let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let request = Frame::new(MessageType::SessionClose, request_id, encode_session_close(Some("client closed")));
            // Best effort: the lease is released by the frame, the shutdown
            // below guarantees the server sees EOF even if the reply is lost.
            let _ = link.write_frame(&request);
            link.shutdown();
        }
        state.link = None;
        state.closed = true;
        state.credential = None;
        state.session_token = None;
        Ok(())
    }

    fn drop_link(&self) {
        let mut state = self.lock();
        if let Some(mut link) = state.link.take() {
            link.shutdown();
        }
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn expect_ok(&self, request: MessageType, body: Vec<u8>, expected: MessageType) -> Result<()> {
        let response = self.round_trip(request, body)?;
        expect_type(&response, expected)
    }

    fn hello(&self) -> Result<String> {
        let response = self.round_trip(MessageType::Hello, Vec::new())?;
        expect_type(&response, MessageType::HelloOk)?;
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }

    fn round_trip(&self, msg_type: MessageType, body: Vec<u8>) -> Result<Frame> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Frame::new(msg_type, request_id, body);
        let mut state = self.lock();
        if state.closed {
            return Err(Error::SessionClosed);
        }
        if state.link.is_none() {
            return Err(Error::Protocol { message: "client transport is not connected — call reconnect() first".to_owned() });
        }
        let exchanged = {
            let link = state.link.as_mut().expect("checked above");
            link.write_frame(&request).and_then(|()| link.read_frame())
        };
        let response = match exchanged {
            Ok(response) => response,
            Err(error) => {
                // The transport state is unknown after an I/O or framing
                // failure; drop it so the next call reports not-connected and
                // the caller decides whether a reconnect is safe.
                state.link = None;
                return Err(error);
            }
        };
        if response.request_id != request_id {
            state.link = None;
            return Err(Error::Corrupt("wire response request_id mismatch"));
        }
        if response.msg_type == MessageType::Error {
            let error = decode_error(&response.body)?;
            return Err(Error::Remote { code: error.code, message: error.message });
        }
        Ok(response)
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("endpoint", &self.endpoint).field("transport", &self.transport).finish_non_exhaustive()
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "yydb-client({})", self.endpoint)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(mut link) = state.link.take() {
                link.shutdown();
            }
            state.closed = true;
        }
    }
}

/// A client-side projection of one VOS table.
///
/// Every method builds a VOS source and executes it through the shared
/// `Prepare` / `ExecutePrepared` frames; there is no second execution engine in
/// the client.
pub struct Table<'a> {
    client: &'a Client,
    name: String,
}

impl Table<'_> {
    /// Table name this handle projects.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Insert one row built from named columns.
    pub fn insert(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        if values.is_empty() {
            return Err(Error::QueryBind { parameter: String::new(), message: "insert requires at least one column".to_owned() });
        }
        let mut fields = Vec::with_capacity(values.len());
        for column in values.keys() {
            validate_identifier(column)?;
            fields.push(format!("{column}: ${column}"));
        }
        let source = format!("{}.insert({{ {} }})", self.name, fields.join(", "));
        let statement = self.client.prepare(&source)?;
        self.client.execute_prepared(&statement, values)?;
        Ok(())
    }

    /// Collect every row of the table.
    pub fn all(&self) -> Result<Vec<Row>> {
        self.collect(&format!("{}.all()", self.name))
    }

    /// Collect at most `limit` rows using the VOS `take` operator.
    pub fn take(&self, limit: usize) -> Result<Vec<Row>> {
        self.collect(&format!("{}.all().take({limit})", self.name))
    }

    /// Collect rows whose `field` equals `value`.
    pub fn filter_eq(&self, field: &str, value: Value) -> Result<Vec<Row>> {
        validate_identifier(field)?;
        let source = format!("{}.all().filter(row => row.{field} == $value).collect()", self.name);
        let params = BTreeMap::from([("value".to_owned(), value)]);
        let statement = self.client.prepare(&source)?;
        self.client.execute_prepared(&statement, &params)
    }

    fn collect(&self, pipeline: &str) -> Result<Vec<Row>> {
        let statement = self.client.prepare(&format!("{pipeline}.collect()"))?;
        self.client.execute_prepared(&statement, &BTreeMap::new())
    }
}

fn validate_identifier(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next()
    else {
        return Err(Error::Protocol { message: "empty VOS identifier".to_owned() });
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(Error::Protocol { message: format!("VOS identifier `{name}` must start with a letter or `_`") });
    }
    if !chars.all(|character| character.is_ascii_alphanumeric() || character == '_') {
        return Err(Error::Protocol { message: format!("VOS identifier `{name}` contains unsupported characters") });
    }
    Ok(())
}

fn expect_type(frame: &Frame, expected: MessageType) -> Result<()> {
    if frame.msg_type == expected { Ok(()) } else { Err(Error::Corrupt("unexpected wire response type")) }
}

fn normalize_endpoint(endpoint: String) -> Result<(TransportKind, String)> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(Error::Unsupported("empty serve endpoint"));
    }
    if endpoint.starts_with("wss://") {
        return Err(Error::Unsupported("wss:// requires TLS, which this client does not provide"));
    }
    let (transport, trimmed) = if let Some(rest) = endpoint.strip_prefix("ws://") {
        (TransportKind::WebSocket, rest)
    }
    else {
        let rest = endpoint.strip_prefix("tcp://").or_else(|| endpoint.strip_prefix("http://")).unwrap_or(endpoint);
        (TransportKind::Tcp, rest)
    };
    let trimmed = trimmed.split('/').next().unwrap_or(trimmed);
    // Validate host:port shape.
    let _ = split_bind(trimmed)?;
    Ok((transport, trimmed.to_owned()))
}

// ─── WebSocket transport ──────────────────────────────────────────────────

fn websocket_handshake(stream: &mut TcpStream, endpoint: &str) -> Result<()> {
    let mut nonce = [0_u8; 16];
    boost_random::Secure::system().fill(&mut nonce).map_err(|error| Error::Protocol { message: format!("websocket nonce: {error}") })?;
    let key = data_encoding::representation::Base64::standard_strict().encode_text(&nonce);
    let request = format!(
        "GET /wire HTTP/1.1\r\nHost: {endpoint}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    use std::io::{Read, Write};
    stream.write_all(request.as_bytes()).map_err(Error::Io)?;
    stream.flush().map_err(Error::Io)?;

    let mut response = Vec::new();
    let mut chunk = [0_u8; 256];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).map_err(Error::Io)?;
        if read == 0 {
            return Err(Error::Protocol { message: "websocket handshake closed before headers completed".to_owned() });
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() > 64 * 1024 {
            return Err(Error::Protocol { message: "websocket handshake exceeded 64 KiB".to_owned() });
        }
    }
    let head = String::from_utf8_lossy(&response);
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 101 ") {
        return Err(Error::Protocol { message: format!("websocket upgrade refused: {status}") });
    }
    let accept = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("Sec-WebSocket-Accept").then(|| value.trim().to_owned())
        })
        .ok_or_else(|| Error::Protocol { message: "websocket upgrade omitted Sec-WebSocket-Accept".to_owned() })?;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let expected = data_encoding::representation::Base64::standard_strict().encode_text(hasher.finalize());
    if accept != expected {
        return Err(Error::Protocol { message: "websocket upgrade returned the wrong Sec-WebSocket-Accept".to_owned() });
    }
    Ok(())
}

fn write_websocket_binary(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    write_websocket_frame(stream, 0x2, payload)
}

fn write_websocket_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut mask = [0_u8; 4];
    boost_random::Secure::system().fill(&mut mask).map_err(|error| Error::Protocol { message: format!("websocket mask: {error}") })?;

    let mut header = Vec::with_capacity(14);
    header.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        header.push(0x80 | len as u8);
    }
    else if len <= u16::MAX as usize {
        header.push(0x80 | 126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    }
    else {
        header.push(0x80 | 127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    header.extend_from_slice(&mask);
    let mut masked = Vec::with_capacity(payload.len());
    masked.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));

    stream.write_all(&header).map_err(Error::Io)?;
    stream.write_all(&masked).map_err(Error::Io)?;
    stream.flush().map_err(Error::Io)?;
    Ok(())
}

fn read_websocket_binary(stream: &mut TcpStream) -> Result<Vec<u8>> {
    loop {
        use std::io::Read;
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).map_err(Error::Io)?;
        let opcode = header[0] & 0x0f;
        let masked = (header[1] & 0x80) != 0;
        let mut len = (header[1] & 0x7f) as u64;
        if len == 126 {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended).map_err(Error::Io)?;
            len = u16::from_be_bytes(extended) as u64;
        }
        else if len == 127 {
            let mut extended = [0_u8; 8];
            stream.read_exact(&mut extended).map_err(Error::Io)?;
            len = u64::from_be_bytes(extended);
        }
        if len > database_protocol_yy::MAX_BODY_LEN as u64 + database_protocol_yy::HEADER_LEN as u64 {
            return Err(Error::Protocol { message: "websocket frame exceeds the wire body limit".to_owned() });
        }
        let mut mask = [0_u8; 4];
        if masked {
            stream.read_exact(&mut mask).map_err(Error::Io)?;
        }
        let mut payload = vec![0_u8; len as usize];
        if len > 0 {
            stream.read_exact(&mut payload).map_err(Error::Io)?;
        }
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        match opcode {
            0x2 | 0x1 => return Ok(payload),
            0x8 => return Err(Error::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "websocket peer closed"))),
            0x9 => write_websocket_frame(stream, 0xA, &payload)?,
            0x0 => return Err(Error::Protocol { message: "websocket continuation frames are not supported".to_owned() }),
            other => return Err(Error::Protocol { message: format!("unsupported websocket opcode {other}") }),
        }
    }
}

/// Client crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_keep_tcp_and_websocket_distinct() {
        assert_eq!(normalize_endpoint("127.0.0.1:7700".to_owned()).expect("tcp"), (TransportKind::Tcp, "127.0.0.1:7700".to_owned()));
        assert_eq!(
            normalize_endpoint("tcp://127.0.0.1:7700".to_owned()).expect("tcp prefix"),
            (TransportKind::Tcp, "127.0.0.1:7700".to_owned())
        );
        assert_eq!(
            normalize_endpoint("ws://127.0.0.1:7700/wire".to_owned()).expect("ws"),
            (TransportKind::WebSocket, "127.0.0.1:7700".to_owned())
        );
        assert!(matches!(normalize_endpoint("wss://127.0.0.1:7700".to_owned()), Err(Error::Unsupported(_))));
        assert!(matches!(normalize_endpoint("  ".to_owned()), Err(Error::Unsupported(_))));
    }

    #[test]
    fn identifiers_are_validated_before_they_reach_vos() {
        for valid in ["Item", "_audit", "a1"] {
            validate_identifier(valid).expect("valid identifier");
        }
        for invalid in ["", "1Item", "Item;drop", "Item.all()", "表"] {
            assert!(validate_identifier(invalid).is_err(), "`{invalid}` must be rejected");
        }
    }

    #[test]
    fn table_insert_projects_named_columns_into_one_vos_source() {
        let values = BTreeMap::from([("name".to_owned(), Value::Text("widget".to_owned())), ("qty".to_owned(), Value::I64(7))]);
        let mut fields = Vec::new();
        for column in values.keys() {
            validate_identifier(column).expect("column");
            fields.push(format!("{column}: ${column}"));
        }
        assert_eq!(format!("Item.insert({{ {} }})", fields.join(", ")), "Item.insert({ name: $name, qty: $qty })");
    }
}
