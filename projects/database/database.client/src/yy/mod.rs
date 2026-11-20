//! Rust **remote** client for **`yydb serve`** (YY wire `YYDB`+`0000` over TCP).
//!
//! This is the non-embedded Rust surface. For in-process access use the
//! [`yydb`] embed facade. Browser and Node TypeScript hosts should use
//! `@yydb/yydb-client`. The all-in-one Node product is `@yydb/yydb` (CLI + engine + serve).
//!
//! YYDB serve has **no ACL**. Connect only to loopback endpoints you control.

#![deny(missing_docs)]

use std::{
    fmt,
    net::TcpStream,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

pub use database_types::{Error, Result, SchemaVersion, Value};
pub use database_udf::HostMicroDefinition;

use database_protocol_yy::{
    Frame, MessageType, decode_error, decode_execute_prepared_ok, decode_kv_get_ok, decode_prepare_ok, decode_rows, decode_scalar_call_ok,
    decode_schema_get_ok, encode_authenticate, encode_execute_prepared, encode_kv_get, encode_kv_put, encode_micro_register, encode_params,
    encode_prepare, encode_savepoint_name, encode_scalar_call, encode_schema_ensure, read_frame, split_bind, write_frame,
};

/// Handle to a remote YYDB server started with `yydb serve`.
pub struct Client {
    endpoint: String,
    stream: Mutex<TcpStream>,
    next_id: AtomicU32,
}

impl Client {
    /// Connect to a serve endpoint such as `127.0.0.1:7700`.
    ///
    /// Strips optional `tcp://` / `http://` / `ws://` prefixes; opens a TCP
    /// socket and performs a `Hello` handshake.
    pub fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = normalize_endpoint(endpoint.into())?;
        let stream = TcpStream::connect(&endpoint)?;
        let client = Self { endpoint, stream: Mutex::new(stream), next_id: AtomicU32::new(1) };
        let _ = client.hello()?;
        Ok(client)
    }

    /// Connect and authenticate with a bootstrap token in the same handshake.
    ///
    /// Equivalent to [`Self::connect`] followed by [`Self::authenticate`].
    pub fn connect_with_token(endpoint: impl Into<String>, token: &str) -> Result<Self> {
        let client = Self::connect(endpoint)?;
        client.authenticate(token)?;
        Ok(client)
    }

    /// Attach a session to this connection with a bootstrap token.
    pub fn authenticate(&self, token: &str) -> Result<()> {
        let response = self.round_trip(MessageType::Authenticate, encode_authenticate(token))?;
        expect_type(&response, MessageType::AuthenticateOk)?;
        Ok(())
    }

    /// Configured endpoint string (`host:port`).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
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
    pub fn ensure_schema(&self, document: &str) -> Result<()> {
        let response = self.round_trip(MessageType::SchemaEnsure, encode_schema_ensure(0, document))?;
        expect_type(&response, MessageType::SchemaEnsureOk)?;
        Ok(())
    }

    /// Fetch remote info text (`path=…\\n…`).
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
    pub fn execute_prepared(
        &self,
        statement_id: &str,
        params: &std::collections::BTreeMap<String, Value>,
    ) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
        let encoded = if params.is_empty() { None } else { Some(encode_params(params)?) };
        let response = self.round_trip(MessageType::ExecutePrepared, encode_execute_prepared(statement_id, encoded.as_deref()))?;
        expect_type(&response, MessageType::ExecutePreparedOk)?;
        decode_rows(&decode_execute_prepared_ok(&response.body)?)
    }

    /// Prepare and execute in one call.
    pub fn query(&self, source: &str, params: &std::collections::BTreeMap<String, Value>) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
        let statement_id = self.prepare(source)?;
        self.execute_prepared(&statement_id, params)
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
        let mut stream = self.stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *stream, &request)?;
        let response = read_frame(&mut *stream)?;
        if response.request_id != request_id {
            return Err(Error::Corrupt("wire response request_id mismatch"));
        }
        if response.msg_type == MessageType::Error {
            let error = decode_error(&response.body)?;
            return Err(Error::Remote { code: error.code, message: error.message });
        }
        Ok(response)
    }
}

fn expect_type(frame: &Frame, expected: MessageType) -> Result<()> {
    if frame.msg_type == expected { Ok(()) } else { Err(Error::Corrupt("unexpected wire response type")) }
}

fn normalize_endpoint(endpoint: String) -> Result<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(Error::Unsupported("empty serve endpoint"));
    }
    let trimmed = endpoint
        .strip_prefix("tcp://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .or_else(|| endpoint.strip_prefix("ws://"))
        .unwrap_or(endpoint);
    let trimmed = trimmed.split('/').next().unwrap_or(trimmed);
    // Validate host:port shape.
    let _ = split_bind(trimmed)?;
    Ok(trimmed.to_owned())
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "yydb-client({})", self.endpoint)
    }
}

/// Client crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
