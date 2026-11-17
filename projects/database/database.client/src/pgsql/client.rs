//! One socket, one connection, no global state.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    ops::Range,
    time::Duration,
};

use super::{
    auth::{ScramClient, md5_password},
    error::{Notice, PostgresError},
};
use database_protocol_pgsql::{self as protocol, MAX_MESSAGE_BYTES};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Read-ahead size of one connection.
///
/// A reply is many small frames, and one `read` per frame would cost two
/// syscalls per row. The socket is drained in 64 KiB chunks instead and each
/// frame is decoded in place, so a thousand-row result costs a handful of
/// reads rather than two thousand.
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// The socket plus the read-ahead buffer frames are decoded from.
#[derive(Debug)]
struct Transport {
    stream: TcpStream,
    buffer: Vec<u8>,
    /// Offset of the frame being decoded, always the first unconsumed byte.
    position: usize,
    /// Number of received bytes in `buffer`.
    filled: usize,
}

impl Transport {
    fn new(stream: TcpStream) -> Self {
        Self { stream, buffer: vec![0; READ_BUFFER_BYTES], position: 0, filled: 0 }
    }

    /// Makes at least `needed` unread bytes contiguous at the returned offset,
    /// refilling the buffer only after the bytes already held are exhausted.
    ///
    /// The bytes there are valid until the next call, which may move them.
    fn ensure(&mut self, needed: usize) -> Result<usize, PostgresError> {
        loop {
            if self.filled - self.position >= needed {
                return Ok(self.position);
            }
            if self.position > 0 {
                self.buffer.copy_within(self.position..self.filled, 0);
                self.filled -= self.position;
                self.position = 0;
            }
            if self.filled == self.buffer.len() {
                // One frame larger than the read-ahead buffer: grow once.
                self.buffer.resize(self.filled + READ_BUFFER_BYTES.max(needed - self.filled), 0);
            }
            let read = self.stream.read(&mut self.buffer[self.filled..]).map_err(io_error)?;
            if read == 0 {
                return Err(PostgresError::Io("the server closed the connection before the message was complete".into()));
            }
            self.filled += read;
        }
    }
}

/// Where to connect and which identity to present in the startup packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub password: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 5432, user: "postgres".into(), database: "postgres".into(), password: None }
    }
}

impl ClientConfig {
    pub fn new(host: impl Into<String>, port: u16, user: impl Into<String>, database: impl Into<String>) -> Self {
        Self { host: host.into(), port, user: user.into(), database: database.into(), password: None }
    }

    /// Supplies a password for MD5, cleartext or SCRAM-SHA-256 authentication.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }
}

/// One column of a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
}

/// Everything one statement returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    pub tag: String,
}

impl QueryResult {
    pub fn column_names(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|column| column.name.as_str())
    }

    /// Cell text for one coordinate, or `None` when the cell is SQL `NULL`.
    pub fn cell(&self, row: usize, column: usize) -> Option<Option<&str>> {
        self.rows.get(row).and_then(|values| values.get(column)).map(|value| value.as_deref())
    }
}

/// One result column together with the type OID the server assigned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedColumn {
    /// Column label from `RowDescription`.
    pub name: String,
    /// PostgreSQL type OID from `RowDescription`.
    pub type_oid: u32,
}

/// Result set that keeps each column's type OID.
///
/// The simple-query path only carries labels, so a caller that has to decode
/// text cells back into typed values needs the extended protocol's
/// `RowDescription` type OIDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypedResult {
    /// Columns with their type OIDs.
    pub columns: Vec<TypedColumn>,
    /// Row-major cells; `None` is SQL `NULL`.
    pub rows: Vec<Vec<Option<String>>>,
    /// `CommandComplete` tag.
    pub tag: String,
}

impl TypedResult {
    /// Drops the type OIDs when only the textual result is needed.
    pub fn into_query_result(self) -> QueryResult {
        QueryResult { columns: self.columns.into_iter().map(|column| Column { name: column.name }).collect(), rows: self.rows, tag: self.tag }
    }
}

/// A named prepared statement living on one connection's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Server-side statement name, unique per connection.
    pub name: String,
    /// Parameter OIDs the server resolved, in declaration order.
    pub parameters: Vec<u32>,
    /// Columns the statement returns; empty when it returns no row set.
    pub columns: Vec<Column>,
}

/// One bound parameter, either text or binary, and `None` for SQL `NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param<'a> {
    /// `0` for text and `1` for binary.
    pub format: u16,
    pub value: Option<&'a [u8]>,
}

impl<'a> Param<'a> {
    /// A text-format parameter. `None` binds SQL `NULL`.
    pub fn text(value: Option<&'a str>) -> Self {
        Self { format: 0, value: value.map(str::as_bytes) }
    }

    /// A binary-format parameter carrying already encoded bytes.
    pub fn binary(value: &'a [u8]) -> Self {
        Self { format: 1, value: Some(value) }
    }

    /// SQL `NULL` with the default text format.
    pub fn null() -> Self {
        Self { format: 0, value: None }
    }
}

/// A named portal bound to a prepared statement and reusable across executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portal {
    pub name: String,
    /// Parameter OIDs the statement resolved.
    pub parameters: Vec<u32>,
    /// Columns the portal returns.
    pub columns: Vec<Column>,
}

/// One `Execute` round trip on a portal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortalReport {
    /// Rows produced by this execute. A suspended portal reports the partial
    /// batch with an empty command tag.
    pub results: Vec<QueryResult>,
    /// `true` when the server stopped at the row limit and the portal is still
    /// open for the next execute.
    pub suspended: bool,
}

/// The `pid` and `secret key` the server sent in `BackendKeyData`.
///
/// Both values appear verbatim in a `CancelRequest`, so they are kept for the
/// lifetime of the session instead of being discarded during startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendKeyData {
    pub process_id: i32,
    pub secret_key: i32,
}

/// One `NotificationResponse` delivered to a listening session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Backend pid that sent the `NOTIFY`.
    pub process_id: i32,
    /// Channel name.
    pub channel: String,
    /// Payload, empty when the `NOTIFY` carried none.
    pub payload: String,
}

/// A live protocol-v3 connection in the simple-query mode.
#[derive(Debug)]
pub struct Connection {
    reader: Transport,
    config: ClientConfig,
    backend_key: Option<BackendKeyData>,
    session_tag: u32,
    statements: u64,
    notices: Vec<Notice>,
    /// `NotificationResponse` frames received so far, in wire order.
    notifications: Vec<Notification>,
    /// Columns of each bound portal, so a suspended `Execute` does not ask the
    /// server to describe — and therefore re-plan — the statement again.
    portal_columns: HashMap<String, Vec<protocol::ColumnDescription>>,
    /// Columns each open prepared statement returns, captured by [`Self::prepare`].
    ///
    /// A prepared statement is described once; every later execute reuses that
    /// description instead of asking the server for a `RowDescription` again.
    prepared_columns: HashMap<String, Vec<protocol::ColumnDescription>>,
    /// Set once the stream is desynchronised, so later calls cannot read the
    /// reply of an abandoned round trip as if it belonged to a new statement.
    failure: Option<String>,
}

/// One simple-query round trip that keeps the statements that already finished.
///
/// `execute` throws the error and drops them. [`Report`] keeps both, matching
/// the order the server sent: every completed result first, then the error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub results: Vec<QueryResult>,
    pub error: Option<PostgresError>,
    pub notices: Vec<Notice>,
}

/// One `COPY ... FROM STDIN` round trip.
///
/// `tag` is the `CommandComplete` tag, such as `COPY 2`. `error` keeps the
/// server error instead of throwing it, because the copy may have failed after
/// the client already streamed rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyInReport {
    pub tag: String,
    pub error: Option<PostgresError>,
    pub notices: Vec<Notice>,
}

/// One `COPY ... TO STDOUT` round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyOutReport {
    /// Every `CopyData` payload concatenated in wire order.
    pub data: Vec<u8>,
    pub tag: String,
    pub error: Option<PostgresError>,
    pub notices: Vec<Notice>,
}

/// Tag and error shared by both copy directions.
struct CopyOutcome {
    tag: String,
    error: Option<PostgresError>,
}

/// Response of one extended-protocol round trip.
struct ExtendedOutcome {
    results: Vec<TypedResult>,
    parameters: Vec<u32>,
    described: Vec<Column>,
    /// Wire description behind `described`, reused by later executes.
    description: Vec<protocol::ColumnDescription>,
    suspended: bool,
}

/// One simple-query reply once `ReadyForQuery` arrived.
struct SimpleOutcome {
    results: Vec<QueryResult>,
    error: Option<PostgresError>,
}

impl Connection {
    /// Opens the socket and completes the startup handshake.
    pub fn open(config: &ClientConfig) -> Result<Self, PostgresError> {
        let address = resolve_address(config)?;
        let stream = TcpStream::connect_timeout(&address, TIMEOUT)
            .map_err(|error| PostgresError::Io(format!("{}:{} cannot be reached: {error}", config.host, config.port)))?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(io_error)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(io_error)?;
        stream.set_nodelay(true).map_err(io_error)?;
        let session_tag = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.subsec_nanos()).unwrap_or(0);
        let mut connection = Self {
            reader: Transport::new(stream),
            config: config.clone(),
            backend_key: None,
            session_tag,
            statements: 0,
            notices: Vec::new(),
            notifications: Vec::new(),
            portal_columns: HashMap::new(),
            prepared_columns: HashMap::new(),
            failure: None,
        };
        connection.startup(config)?;
        Ok(connection)
    }

    /// The `BackendKeyData` sent during startup, or `None` when the server
    /// skipped it.
    pub fn backend_key_data(&self) -> Option<BackendKeyData> {
        self.backend_key
    }

    /// Sends one `CancelRequest` on a fresh socket, then closes it.
    ///
    /// PostgreSQL answers nothing on this connection, so `Ok` means the packet
    /// left this process. The main socket is untouched and stays usable.
    pub fn cancel(&self) -> Result<(), PostgresError> {
        let Some(key) = self.backend_key
        else {
            return Err(PostgresError::Protocol("server sent no BackendKeyData to cancel".into()));
        };
        let address = resolve_address(&self.config)?;
        let mut stream = TcpStream::connect_timeout(&address, TIMEOUT)
            .map_err(|error| PostgresError::Io(format!("cancel connection to {}:{} failed: {error}", self.config.host, self.config.port)))?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(io_error)?;
        stream.write_all(&protocol::cancel_request_packet(key.process_id, key.secret_key)).map_err(io_error)?;
        stream.shutdown(std::net::Shutdown::Both).map_err(io_error)?;
        Ok(())
    }

    fn ensure_usable(&self) -> Result<(), PostgresError> {
        match &self.failure {
            Some(reason) => Err(PostgresError::Io(format!("connection is unusable after {reason}"))),
            None => Ok(()),
        }
    }

    /// Marks the stream as unusable after a transport or protocol failure.
    ///
    /// A server error is not a desync: the server answered and the next
    /// `ReadyForQuery` already drained the stream, so the session stays usable.
    fn poison(&mut self, error: &PostgresError) {
        if !matches!(error, PostgresError::Io(_) | PostgresError::Protocol(_)) {
            return;
        }
        if self.failure.is_none() {
            self.failure = Some(error.to_string());
        }
    }

    fn startup(&mut self, config: &ClientConfig) -> Result<(), PostgresError> {
        self.reader.stream.write_all(&protocol::startup_packet(&config.user, &config.database)).map_err(io_error)?;
        let mut authenticated = false;
        let mut scram: Option<ScramClient> = None;
        loop {
            let (tag, range) = self.read_message()?;
            let payload = &self.reader.buffer[range];
            match tag {
                b'E' => return Err(protocol::error_response(payload)),
                b'R' => {
                    if payload.len() < 4 {
                        return Err(PostgresError::Protocol("truncated PostgreSQL authentication request".into()));
                    }
                    let code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if let Some(answer) = authentication_answer(config, code, &payload[4..], &mut scram, &mut authenticated)? {
                        self.reader.stream.write_all(&answer).map_err(io_error)?;
                    }
                }
                b'Z' => {
                    if !authenticated {
                        return Err(PostgresError::Protocol("ReadyForQuery arrived before authentication".into()));
                    }
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok(());
                }
                b'K' => {
                    if payload.len() == 8 {
                        self.backend_key = Some(BackendKeyData {
                            process_id: i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                            secret_key: i32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
                        });
                    }
                }
                b'S' => {}
                b'N' => record_notice(&mut self.notices, payload)?,
                other => return Err(PostgresError::Protocol(format!("unsupported PostgreSQL startup response tag {other}"))),
            }
        }
    }

    /// Runs one simple-query statement and returns every result it produced.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, PostgresError> {
        self.ensure_usable()?;
        let outcome = match self.run_simple(sql) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        match outcome.error {
            Some(error) => Err(error),
            None => Ok(outcome.results),
        }
    }

    /// Runs one simple-query batch and keeps the statements that already finished.
    ///
    /// The reply order is the server order: every completed result first, then
    /// the error, then every notice the server emitted for this batch. Notices
    /// of earlier batches stay buffered for `take_notices`.
    pub fn execute_report(&mut self, sql: &str) -> Result<Report, PostgresError> {
        self.ensure_usable()?;
        let start = self.notices.len();
        let outcome = match self.run_simple(sql) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        let notices = self.notices.split_off(start);
        Ok(Report { results: outcome.results, error: outcome.error, notices })
    }

    /// Drains every notice received so far, in wire order.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// Drains every `NOTIFY` delivered to this session so far, in wire order.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.notifications)
    }

    fn run_simple(&mut self, sql: &str) -> Result<SimpleOutcome, PostgresError> {
        self.reader.stream.write_all(&protocol::query_packet(sql)?).map_err(io_error)?;
        let mut results = Vec::new();
        let mut current: Option<(Vec<String>, Vec<Vec<Option<String>>>)> = None;
        let mut failure: Option<PostgresError> = None;
        loop {
            let (tag, range) = self.read_message()?;
            let payload = &self.reader.buffer[range];
            if failure.is_some() {
                // The server abandons the remaining statements after an error and
                // only sends ReadyForQuery next; drain it so the socket stays usable.
                if tag == b'Z' {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok(SimpleOutcome { results, error: failure });
                }
                continue;
            }
            match tag {
                b'E' => {
                    failure = Some(protocol::error_response(payload));
                }
                b'T' => {
                    if current.is_some() {
                        return Err(PostgresError::Protocol("unexpected PostgreSQL RowDescription".into()));
                    }
                    current = Some((protocol::column_names(payload)?, Vec::new()));
                }
                b'D' => {
                    let Some((columns, rows)) = current.as_mut()
                    else {
                        return Err(PostgresError::Protocol("DataRow without RowDescription".into()));
                    };
                    rows.push(protocol::data_row(payload, columns.len())?);
                }
                b'C' => {
                    let command = protocol::command_tag(payload)?;
                    let (columns, rows) = current.take().unwrap_or_default();
                    results.push(QueryResult { columns: columns.into_iter().map(|name| Column { name }).collect(), rows, tag: command });
                }
                b'I' => {
                    if !payload.is_empty() || current.is_some() {
                        return Err(PostgresError::Protocol("invalid EmptyQueryResponse".into()));
                    }
                }
                b'Z' => {
                    if current.is_some() {
                        return Err(PostgresError::Protocol("PostgreSQL result ended before CommandComplete".into()));
                    }
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok(SimpleOutcome { results, error: None });
                }
                b'S' | b'K' => {}
                b'N' => record_notice(&mut self.notices, payload)?,
                b'A' => record_notification(&mut self.notifications, payload)?,
                other => return Err(PostgresError::Protocol(format!("unsupported PostgreSQL response tag {other}"))),
            }
        }
    }

    /// Runs one parameterised statement through the extended protocol using the
    /// unnamed prepared statement, so nothing stays on the server afterwards.
    pub fn execute_with_params(&mut self, sql: &str, params: &[Option<&str>], param_types: &[u32]) -> Result<Vec<QueryResult>, PostgresError> {
        let params: Vec<Param<'_>> = params.iter().map(|value| Param::text(*value)).collect();
        self.execute_with_param_formats(sql, &params, param_types, &[])
    }

    /// Runs one parameterised statement with explicit text or binary parameters
    /// and result formats.
    ///
    /// `result_formats` follows the wire shape: empty for text columns, one
    /// code for every column, or one code per column.
    pub fn execute_with_param_formats(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
        param_types: &[u32],
        result_formats: &[u16],
    ) -> Result<Vec<QueryResult>, PostgresError> {
        let results = self.execute_with_param_formats_typed(sql, params, param_types, result_formats)?;
        Ok(results.into_iter().map(TypedResult::into_query_result).collect())
    }

    /// Extended-protocol round trip that keeps the result columns' type OIDs.
    ///
    /// Same wire exchange as [`Connection::execute_with_param_formats`]; the
    /// only difference is that `RowDescription` type OIDs survive instead of
    /// being reduced to labels.
    pub fn execute_with_param_formats_typed(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
        param_types: &[u32],
        result_formats: &[u16],
    ) -> Result<Vec<TypedResult>, PostgresError> {
        self.ensure_usable()?;
        let param_formats = param_format_codes(params);
        let values: Vec<Option<&[u8]>> = params.iter().map(|param| param.value).collect();
        let mut packet = protocol::parse_packet("", sql, param_types)?;
        packet.extend_from_slice(&protocol::bind_packet_with_formats("", "", &values, &param_formats, result_formats)?);
        packet.extend_from_slice(&protocol::describe_packet(b'P', "")?);
        packet.extend_from_slice(&protocol::execute_packet("", 0)?);
        packet.extend_from_slice(&protocol::sync_packet());
        match self.read_extended(&packet, None) {
            Ok(outcome) => Ok(outcome.results),
            Err(error) => {
                self.poison(&error);
                Err(error)
            }
        }
    }

    /// Prepares a named statement and describes it in the same round trip.
    pub fn prepare(&mut self, sql: &str, param_types: &[u32]) -> Result<Prepared, PostgresError> {
        self.ensure_usable()?;
        self.statements += 1;
        let name = format!("pg_rs_{:08x}_{}", self.session_tag, self.statements);
        let mut packet = protocol::parse_packet(&name, sql, param_types)?;
        packet.extend_from_slice(&protocol::describe_packet(b'S', &name)?);
        packet.extend_from_slice(&protocol::sync_packet());
        let outcome = match self.read_extended(&packet, None) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        self.prepared_columns.insert(name.clone(), outcome.description.clone());
        Ok(Prepared { name, parameters: outcome.parameters, columns: outcome.described })
    }

    /// Binds parameters to an existing prepared statement and executes it.
    pub fn execute_prepared(&mut self, name: &str, params: &[Option<&str>]) -> Result<Vec<QueryResult>, PostgresError> {
        let params: Vec<Param<'_>> = params.iter().map(|value| Param::text(*value)).collect();
        self.execute_prepared_formats(name, &params, &[])
    }

    /// Executes an existing prepared statement with explicit parameter and
    /// result formats. The statement stays on the server for the next call.
    pub fn execute_prepared_formats(
        &mut self,
        name: &str,
        params: &[Param<'_>],
        result_formats: &[u16],
    ) -> Result<Vec<QueryResult>, PostgresError> {
        self.ensure_usable()?;
        let param_formats = param_format_codes(params);
        let values: Vec<Option<&[u8]>> = params.iter().map(|param| param.value).collect();
        let mut packet = protocol::bind_packet_with_formats("", name, &values, &param_formats, result_formats)?;
        // `prepare` already described the statement, so the columns are known:
        // asking for the portal description again would make the server rebuild
        // and resend a `RowDescription` on every execution. Only a binary result
        // format changes the description, and that still needs a fresh one.
        let described = if result_formats.is_empty() { self.prepared_columns.get(name).cloned() } else { None };
        if described.is_none() {
            packet.extend_from_slice(&protocol::describe_packet(b'P', "")?);
        }
        packet.extend_from_slice(&protocol::execute_packet("", 0)?);
        packet.extend_from_slice(&protocol::sync_packet());
        match self.read_extended(&packet, described.as_deref()) {
            Ok(outcome) => Ok(outcome.results.into_iter().map(TypedResult::into_query_result).collect()),
            Err(error) => {
                self.poison(&error);
                Err(error)
            }
        }
    }

    /// Binds a named portal to a prepared statement so it can be executed more
    /// than once through [`Connection::execute_portal`].
    ///
    /// The name is required because an unnamed portal is dropped by `Sync`.
    pub fn bind_portal(
        &mut self,
        statement: &str,
        portal: &str,
        params: &[Param<'_>],
        result_formats: &[u16],
    ) -> Result<Portal, PostgresError> {
        self.ensure_usable()?;
        if portal.is_empty() {
            return Err(PostgresError::Protocol("a reusable portal needs a name".into()));
        }
        let param_formats = param_format_codes(params);
        let values: Vec<Option<&[u8]>> = params.iter().map(|param| param.value).collect();
        let mut packet = protocol::bind_packet_with_formats(portal, statement, &values, &param_formats, result_formats)?;
        packet.extend_from_slice(&protocol::describe_packet(b'P', portal)?);
        packet.extend_from_slice(&protocol::sync_packet());
        let outcome = match self.read_extended(&packet, None) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        self.portal_columns.insert(portal.to_owned(), outcome.description.clone());
        Ok(Portal { name: portal.to_owned(), parameters: outcome.parameters, columns: outcome.described })
    }

    /// Executes an open portal. `max_rows` of `0` runs it to completion, and a
    /// positive limit stops early with `suspended` set.
    ///
    /// Every execute after the first reuses the portal bound by
    /// [`Connection::bind_portal`], so the rows continue where the previous
    /// batch stopped.
    pub fn execute_portal(&mut self, portal: &str, max_rows: i32) -> Result<PortalReport, PostgresError> {
        self.ensure_usable()?;
        // `bind_portal` already described this portal. Describing again would
        // make the server re-plan and re-run the whole statement per batch just
        // to rebuild a `RowDescription`, which is what makes a suspended portal
        // slower than a plain cursor. Only an unknown portal asks again.
        let described = self.portal_columns.get(portal).cloned();
        let mut packet = Vec::new();
        if described.is_none() {
            packet.extend_from_slice(&protocol::describe_packet(b'P', portal)?);
        }
        packet.extend_from_slice(&protocol::execute_packet(portal, max_rows)?);
        packet.extend_from_slice(&protocol::sync_packet());
        let outcome = match self.read_extended(&packet, described.as_deref()) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        Ok(PortalReport { results: outcome.results.into_iter().map(TypedResult::into_query_result).collect(), suspended: outcome.suspended })
    }

    /// Closes a portal on the server. Closing an unknown name still answers
    /// `CloseComplete`.
    pub fn close_portal(&mut self, name: &str) -> Result<(), PostgresError> {
        self.portal_columns.remove(name);
        self.close_named(b'P', name)
    }

    /// Closes a prepared statement on the server. Closing an unknown name is a
    /// no-op: the server answers CloseComplete either way.
    pub fn close_prepared(&mut self, name: &str) -> Result<(), PostgresError> {
        self.prepared_columns.remove(name);
        self.close_named(b'S', name)
    }

    fn close_named(&mut self, target: u8, name: &str) -> Result<(), PostgresError> {
        self.ensure_usable()?;
        let mut packet = protocol::close_packet(target, name)?;
        packet.extend_from_slice(&protocol::sync_packet());
        let outcome = match self.read_extended(&packet, None) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        if !outcome.results.is_empty() {
            return Err(PostgresError::Protocol("Close returned a result set".into()));
        }
        Ok(())
    }

    /// Streams `chunks` as `COPY ... FROM STDIN` data for one statement.
    ///
    /// The chunks are only written once the server answered `CopyInResponse`.
    /// A server error, before or during the copy, ends the round trip with
    /// `ReadyForQuery`, which is drained before returning, so the next call on
    /// the same connection still works.
    pub fn copy_in(&mut self, sql: &str, chunks: &[&[u8]]) -> Result<CopyInReport, PostgresError> {
        self.ensure_usable()?;
        let start = self.notices.len();
        let outcome = match self.run_copy_in(sql, chunks) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        let notices = self.notices.split_off(start);
        Ok(CopyInReport { tag: outcome.tag, error: outcome.error, notices })
    }

    /// Collects every `CopyData` payload of one `COPY ... TO STDOUT` statement.
    ///
    /// A server error keeps whatever data already arrived and is reported in
    /// `error` instead of thrown, so the caller can still inspect the prefix.
    pub fn copy_out(&mut self, sql: &str) -> Result<CopyOutReport, PostgresError> {
        self.ensure_usable()?;
        let start = self.notices.len();
        let (data, outcome) = match self.run_copy_out(sql) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poison(&error);
                return Err(error);
            }
        };
        let notices = self.notices.split_off(start);
        Ok(CopyOutReport { data, tag: outcome.tag, error: outcome.error, notices })
    }

    fn run_copy_in(&mut self, sql: &str, chunks: &[&[u8]]) -> Result<CopyOutcome, PostgresError> {
        self.reader.stream.write_all(&protocol::query_packet(sql)?).map_err(io_error)?;
        let mut tag = String::new();
        let mut failure: Option<PostgresError> = None;
        let mut streaming = false;
        loop {
            let (byte, range) = self.read_message()?;
            let payload = &self.reader.buffer[range];
            if failure.is_some() {
                if byte == b'Z' {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok(CopyOutcome { tag, error: failure });
                }
                continue;
            }
            match byte {
                b'G' if !streaming => {
                    protocol::copy_response(payload)?;
                    for chunk in chunks {
                        self.reader.stream.write_all(&protocol::copy_data_packet(chunk)?).map_err(io_error)?;
                    }
                    self.reader.stream.write_all(&protocol::copy_done_packet()).map_err(io_error)?;
                    streaming = true;
                }
                b'C' => tag = protocol::command_tag(payload)?,
                b'E' => failure = Some(protocol::error_response(payload)),
                b'S' | b'K' => {}
                b'N' => record_notice(&mut self.notices, payload)?,
                b'A' => record_notification(&mut self.notifications, payload)?,
                b'Z' => {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok(CopyOutcome { tag, error: None });
                }
                other => return Err(PostgresError::Protocol(format!("unsupported PostgreSQL copy-in response tag {other}"))),
            }
        }
    }

    fn run_copy_out(&mut self, sql: &str) -> Result<(Vec<u8>, CopyOutcome), PostgresError> {
        self.reader.stream.write_all(&protocol::query_packet(sql)?).map_err(io_error)?;
        let mut data = Vec::new();
        let mut tag = String::new();
        let mut failure: Option<PostgresError> = None;
        loop {
            let (byte, range) = self.read_message()?;
            let payload = &self.reader.buffer[range];
            if failure.is_some() {
                if byte == b'Z' {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok((data, CopyOutcome { tag, error: failure }));
                }
                continue;
            }
            match byte {
                b'H' => {
                    protocol::copy_response(payload)?;
                }
                b'd' => data.extend_from_slice(payload),
                b'c' => {}
                b'C' => tag = protocol::command_tag(payload)?,
                b'E' => failure = Some(protocol::error_response(payload)),
                b'S' | b'K' => {}
                b'N' => record_notice(&mut self.notices, payload)?,
                b'A' => record_notification(&mut self.notifications, payload)?,
                b'Z' => {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    return Ok((data, CopyOutcome { tag, error: None }));
                }
                other => return Err(PostgresError::Protocol(format!("unsupported PostgreSQL copy-out response tag {other}"))),
            }
        }
    }

    fn read_extended(&mut self, packet: &[u8], expected: Option<&[protocol::ColumnDescription]>) -> Result<ExtendedOutcome, PostgresError> {
        self.reader.stream.write_all(packet).map_err(io_error)?;
        let mut results = Vec::new();
        let mut parameters = Vec::new();
        let mut details: Vec<protocol::ColumnDescription> = Vec::new();
        let mut described: Vec<Column> = Vec::new();
        // A round trip that carries a Describe decodes its rows against the
        // `RowDescription` it receives. An execute that reuses the description
        // taken at `prepare` time starts from that one instead, so no
        // `RowDescription` is expected on the wire.
        let mut current: Option<(Vec<protocol::ColumnDescription>, Vec<Vec<Option<String>>>)> =
            expected.map(|columns| (columns.to_vec(), Vec::new()));
        let mut suspended = false;
        let mut failure: Option<PostgresError> = None;
        // A decode error must still drain to ReadyForQuery so the session stays
        // usable, but the error is returned instead of the partial rows.
        let mut deferred: Option<PostgresError> = None;
        loop {
            let (tag, range) = self.read_message()?;
            let payload = &self.reader.buffer[range];
            if failure.is_some() || deferred.is_some() {
                if tag == b'Z' {
                    return Err(failure.or(deferred).expect("failure or deferred is set"));
                }
                continue;
            }
            match tag {
                b'1' | b'2' | b'3' => {
                    if !payload.is_empty() {
                        return Err(PostgresError::Protocol("invalid extended-protocol completion".into()));
                    }
                }
                b't' => parameters = protocol::parameter_description(payload)?,
                b'T' => {
                    let columns = protocol::row_description(payload)?;
                    if let Some((_, rows)) = current.as_ref() {
                        if !rows.is_empty() {
                            return Err(PostgresError::Protocol("unexpected PostgreSQL RowDescription".into()));
                        }
                    }
                    described = columns.iter().map(|column| Column { name: column.name.clone() }).collect();
                    details = columns.clone();
                    current = Some((columns, Vec::new()));
                }
                b'n' => {
                    if let Some((_, rows)) = current.as_ref() {
                        if !rows.is_empty() {
                            return Err(PostgresError::Protocol("unexpected PostgreSQL NoData".into()));
                        }
                    }
                    current = Some((Vec::new(), Vec::new()));
                }
                b'D' => {
                    let Some((columns, rows)) = current.as_mut()
                    else {
                        return Err(PostgresError::Protocol("DataRow without RowDescription".into()));
                    };
                    match protocol::data_row_with_formats(payload, columns) {
                        Ok(values) => rows.push(values),
                        Err(error) => deferred = Some(error),
                    }
                }
                b'C' => {
                    let command = protocol::command_tag(payload)?;
                    let (columns, rows) = current.take().unwrap_or_else(|| (details.clone(), Vec::new()));
                    let columns: Vec<TypedColumn> =
                        columns.iter().map(|column| TypedColumn { name: column.name.clone(), type_oid: column.type_oid }).collect();
                    results.push(TypedResult { columns, rows, tag: command });
                }
                b'I' => {
                    // An empty statement answers Describe with NoData and Execute
                    // with EmptyQueryResponse, so clear that placeholder instead of
                    // treating it as a result set.
                    if !payload.is_empty() {
                        return Err(PostgresError::Protocol("invalid EmptyQueryResponse".into()));
                    }
                    if let Some((columns, rows)) = current.take() {
                        if !columns.is_empty() || !rows.is_empty() {
                            return Err(PostgresError::Protocol("invalid EmptyQueryResponse".into()));
                        }
                    }
                }
                b's' => {
                    let (columns, rows) = current.take().unwrap_or_else(|| (details.clone(), Vec::new()));
                    let columns: Vec<TypedColumn> =
                        columns.iter().map(|column| TypedColumn { name: column.name.clone(), type_oid: column.type_oid }).collect();
                    results.push(TypedResult { columns, rows, tag: String::new() });
                    suspended = true;
                }
                b'E' => failure = Some(protocol::error_response(payload)),
                b'Z' => {
                    if payload.len() != 1 {
                        return Err(PostgresError::Protocol("invalid ReadyForQuery".into()));
                    }
                    // A describe-only round trip (prepare) leaves the RowDescription
                    // pending without a CommandComplete; that is not a result set.
                    if let Some((_, rows)) = current.take() {
                        if !rows.is_empty() {
                            return Err(PostgresError::Protocol("PostgreSQL result ended before CommandComplete".into()));
                        }
                    }
                    return Ok(ExtendedOutcome { results, parameters, described, description: details, suspended });
                }
                b'S' | b'K' => {}
                b'N' => record_notice(&mut self.notices, payload)?,
                b'A' => record_notification(&mut self.notifications, payload)?,
                other => return Err(PostgresError::Protocol(format!("unsupported PostgreSQL response tag {other}"))),
            }
        }
    }

    /// Shuts the socket down without consuming the handle.
    pub fn disconnect(&mut self) -> Result<(), PostgresError> {
        self.reader.stream.flush().map_err(io_error)?;
        self.reader.stream.shutdown(std::net::Shutdown::Both).map_err(io_error)
    }

    /// Closes the socket on the wire instead of waiting for the process to exit.
    pub fn close(mut self) -> Result<(), PostgresError> {
        self.disconnect()
    }

    /// Reads one framed message from the read-ahead buffer.
    ///
    /// Returns the tag and the range its payload occupies inside
    /// `self.reader.buffer`. Nothing is allocated and nothing is copied; the
    /// range is only valid until the next message is read.
    fn read_message(&mut self) -> Result<(u8, Range<usize>), PostgresError> {
        let start = self.reader.ensure(5)?;
        let header = &self.reader.buffer[start..start + 5];
        let tag = header[0];
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length < 4 || length > MAX_MESSAGE_BYTES {
            return Err(PostgresError::Protocol("invalid PostgreSQL message length".into()));
        }
        let payload = length - 4;
        self.reader.position = start + 5;
        let payload_start = self.reader.ensure(payload)?;
        self.reader.position = payload_start + payload;
        Ok((tag, payload_start..payload_start + payload))
    }
}

/// Records one `NoticeResponse` while a decoded frame is still borrowed from the
/// read buffer, so notice handling never re-borrows the whole connection.
fn record_notice(notices: &mut Vec<Notice>, payload: &[u8]) -> Result<(), PostgresError> {
    notices.push(protocol::notice_response(payload)?);
    Ok(())
}

/// Records one `NotificationResponse`: sender pid, then channel and payload as
/// NUL-terminated strings.
fn record_notification(notifications: &mut Vec<Notification>, payload: &[u8]) -> Result<(), PostgresError> {
    if payload.len() < 6 {
        return Err(PostgresError::Protocol("truncated NotificationResponse".into()));
    }
    let process_id = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let channel_end = payload[4..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|relative| 4 + relative)
        .ok_or_else(|| PostgresError::Protocol("NotificationResponse channel is not terminated".into()))?;
    let payload_end = payload[channel_end + 1..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|relative| channel_end + 1 + relative)
        .ok_or_else(|| PostgresError::Protocol("NotificationResponse payload is not terminated".into()))?;
    let channel = std::str::from_utf8(&payload[4..channel_end])
        .map_err(|_| PostgresError::Protocol("NotificationResponse channel is not UTF-8".into()))?;
    let value = std::str::from_utf8(&payload[channel_end + 1..payload_end])
        .map_err(|_| PostgresError::Protocol("NotificationResponse payload is not UTF-8".into()))?;
    notifications.push(Notification { process_id, channel: channel.to_owned(), payload: value.to_owned() });
    Ok(())
}

/// Turns one server authentication request into the packet the client must
/// answer with, or `None` when the request needs no reply.
///
/// Codes come from the protocol-v3 `AuthenticationRequest`: `0` ok, `3`
/// cleartext, `5` MD5, `10` SASL mechanisms, `11` SASL continue, `12` SASL
/// final. Anything else is a request this client cannot satisfy, so it fails
/// loudly instead of leaving the socket half authenticated.
fn authentication_answer(
    config: &ClientConfig,
    code: u32,
    data: &[u8],
    scram: &mut Option<ScramClient>,
    authenticated: &mut bool,
) -> Result<Option<Vec<u8>>, PostgresError> {
    match code {
        0 => {
            if !data.is_empty() {
                return Err(PostgresError::Protocol("invalid AuthenticationOk".into()));
            }
            *authenticated = true;
            Ok(None)
        }
        3 => {
            let password = required_password(config, "cleartext")?;
            Ok(Some(protocol::password_packet(password)?))
        }
        5 => {
            if data.len() != 4 {
                return Err(PostgresError::Protocol("invalid MD5 authentication salt".into()));
            }
            let password = required_password(config, "MD5")?;
            Ok(Some(protocol::password_packet(md5_password(password, &config.user, data))?))
        }
        10 => {
            if !data.split(|byte| *byte == 0).any(|part| part == b"SCRAM-SHA-256") {
                return Err(PostgresError::Unsupported("no supported SASL mechanism offered by the server".into()));
            }
            let password = required_password(config, "SCRAM-SHA-256")?;
            let (client, client_first) = ScramClient::new(&config.user, password)?;
            *scram = Some(client);
            Ok(Some(protocol::sasl_initial_packet("SCRAM-SHA-256", client_first.as_bytes())?))
        }
        11 => {
            let client = scram.as_mut().ok_or(PostgresError::Protocol("SASLContinue arrived before SASLInitialResponse".into()))?;
            let server_first =
                std::str::from_utf8(data).map_err(|_| PostgresError::Protocol("SCRAM server-first is not valid UTF-8".into()))?;
            let client_final = client.continue_with(server_first)?;
            Ok(Some(protocol::sasl_response_packet(client_final.as_bytes())?))
        }
        12 => {
            let client = scram.as_ref().ok_or(PostgresError::Protocol("SASLFinal arrived before SASLInitialResponse".into()))?;
            let server_final =
                std::str::from_utf8(data).map_err(|_| PostgresError::Protocol("SCRAM server-final is not valid UTF-8".into()))?;
            client.verify(server_final)?;
            Ok(None)
        }
        other => Err(PostgresError::Unsupported(format!("PostgreSQL authentication method {other}"))),
    }
}

fn required_password<'a>(config: &'a ClientConfig, method: &str) -> Result<&'a str, PostgresError> {
    config
        .password
        .as_deref()
        .ok_or_else(|| PostgresError::Protocol(format!("PostgreSQL server requested {method} authentication but no password was configured")))
}

/// Wire format codes for a parameter list: empty when every parameter is text,
/// one shared code when they agree, and one code per parameter otherwise.
fn param_format_codes(params: &[Param<'_>]) -> Vec<u16> {
    let Some(first) = params.first()
    else {
        return Vec::new();
    };
    if params.iter().all(|param| param.format == 0) {
        return Vec::new();
    }
    if params.iter().all(|param| param.format == first.format) {
        return vec![first.format];
    }
    params.iter().map(|param| param.format).collect()
}

fn resolve_address(config: &ClientConfig) -> Result<SocketAddr, PostgresError> {
    (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|error| PostgresError::Io(format!("{}:{} cannot be resolved: {error}", config.host, config.port)))?
        .next()
        .ok_or_else(|| PostgresError::Io(format!("{}:{} resolved to no address", config.host, config.port)))
}

fn io_error(error: std::io::Error) -> PostgresError {
    PostgresError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_reads_one_coordinate_and_nulls() {
        let result = QueryResult {
            columns: vec![Column { name: "one".into() }],
            rows: vec![vec![Some("1".into())], vec![None]],
            tag: "SELECT 2".into(),
        };
        assert_eq!(result.cell(0, 0), Some(Some("1")));
        assert_eq!(result.cell(1, 0), Some(None));
        assert_eq!(result.cell(2, 0), None);
        assert_eq!(result.column_names().collect::<Vec<_>>(), vec!["one"]);
    }

    #[test]
    fn default_config_points_at_loopback() {
        assert_eq!(ClientConfig::default().port, 5432);
    }
}
