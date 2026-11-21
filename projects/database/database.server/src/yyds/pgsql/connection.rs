//! PostgreSQL protocol-v3 connection establishment and explicit query rejection.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use super::{
    auth::{AuthFailure, PgAuthentication},
    wire::{FrameError, StartupParameterError, StartupParameters, decode_message, decode_startup, encode_message},
};
use database_execution::pgsql::{
    ABORTED_BLOCK, ExecutionOutcome, PgsqlSqlRuntime, StatementOutcome, TransactionSession, dispatch_statement, normalized_session_syntax,
    pg_sqlstate, run_statement, statement_error,
};
use database_protocol_pgsql::auth::ScramServer;
use database_query::{
    DiscardTarget, SqlBoundExpression, SqlBoundLiteral, SqlCatalogBoundStatement, SqlColumn, SqlDialect, SqlFieldType, SqlParameterStyle,
    SqlSessionCommand, SqlValue, analyze_meta_query, bind_sql_catalog, describe_bound_insert, describe_plan, parse_session_command,
    encode_hex, infer_sql_parameter_types, neutralized_sql_parameters, nulled_sql_parameters, pg_server_version, plan_statement_for_dialect,
    rewrite_double_colon_casts, split_sql_batch, substitute_sql_parameters,
};

/// PostgreSQL type OIDs supported by the gateway.
const OID_BOOL: u32 = 16;
const OID_BYTEA: u32 = 17;
const OID_NAME: u32 = 19;
const OID_INT8: u32 = 20;
const OID_INT2: u32 = 21;
const OID_INT4: u32 = 23;
const OID_TEXT: u32 = 25;
const OID_FLOAT4: u32 = 700;
const OID_FLOAT8: u32 = 701;
const OID_VARCHAR: u32 = 1043;
const OID_DATE: u32 = 1082;
const OID_TIMESTAMP: u32 = 1114;
const OID_TIMESTAMPTZ: u32 = 1184;
const OID_NUMERIC: u32 = 1700;
const OID_UUID: u32 = 2950;
const OID_JSON: u32 = 114;
const OID_JSONB: u32 = 3802;
const OID_TEXT_ARRAY: u32 = 1009;
const OID_VARCHAR_ARRAY: u32 = 1015;
const OID_UUID_ARRAY: u32 = 2951;
const OID_JSONB_ARRAY: u32 = 3807;

const PROTOCOL_V3: u32 = 196_608;
const SSL_REQUEST: u32 = 80_877_103;
const GSSENC_REQUEST: u32 = 80_877_104;
const CANCEL_REQUEST: u32 = 80_877_102;
const MAX_STARTUP_FRAME: usize = 64 * 1024;
const MAX_MESSAGE_FRAME: usize = 16 * 1024 * 1024;
const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;
static NEXT_SECRET: AtomicU32 = AtomicU32::new(1);

/// Establishes an unauthenticated protocol-v3 session, then rejects unsupported SQL explicitly.
///
/// The caller must restrict the peer to loopback until authentication and TLS are implemented.
pub fn serve_connection(stream: TcpStream, runtime: Option<Arc<PgsqlSqlRuntime>>) -> io::Result<()> {
    serve_connection_with_auth(stream, runtime, PgAuthentication::Trust)
}

/// Establishes a protocol-v3 session that authenticates with `auth` before any
/// query runs.
pub fn serve_connection_with_auth(stream: TcpStream, runtime: Option<Arc<PgsqlSqlRuntime>>, auth: PgAuthentication) -> io::Result<()> {
    serve_connection_with_auth_and_tls(stream, runtime, auth, None)
}

/// Establishes a protocol-v3 session that authenticates with `auth` and offers
/// the configured TLS certificate on `SSLRequest`.
pub fn serve_connection_with_auth_and_tls(
    stream: TcpStream,
    runtime: Option<Arc<PgsqlSqlRuntime>>,
    auth: PgAuthentication,
    tls: Option<Arc<super::tls::PgTls>>,
) -> io::Result<()> {
    if !stream.peer_addr()?.ip().is_loopback() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "PostgreSQL accepts loopback connections only"));
    }
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    serve_session_with_tls(Box::new(stream), runtime, Some(Arc::new(auth)), tls)
}

/// Byte stream a protocol-v3 session runs over: a plain socket or a TLS tunnel.
trait SessionStream: Read + Write {}
impl<T: Read + Write> SessionStream for T {}

/// Serves a loopback session with cooperative service-stop cancellation.
pub fn serve_cancellable(stream: TcpStream, stopping: Arc<AtomicBool>, runtime: Option<Arc<PgsqlSqlRuntime>>) -> io::Result<()> {
    serve_cancellable_with_auth(stream, stopping, runtime, PgAuthentication::Trust)
}

/// Serves a loopback session with cooperative service-stop cancellation and the
/// configured authentication method.
pub fn serve_cancellable_with_auth(
    stream: TcpStream,
    stopping: Arc<AtomicBool>,
    runtime: Option<Arc<PgsqlSqlRuntime>>,
    auth: PgAuthentication,
) -> io::Result<()> {
    serve_cancellable_with_auth_and_tls(stream, stopping, runtime, auth, None)
}

/// Serves a loopback session with cooperative service-stop cancellation, the
/// configured authentication method and TLS.
pub fn serve_cancellable_with_auth_and_tls(
    stream: TcpStream,
    stopping: Arc<AtomicBool>,
    runtime: Option<Arc<PgsqlSqlRuntime>>,
    auth: PgAuthentication,
    tls: Option<Arc<super::tls::PgTls>>,
) -> io::Result<()> {
    if !stream.peer_addr()?.ip().is_loopback() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "PostgreSQL accepts loopback connections only"));
    }
    stream.set_nonblocking(true)?;
    serve_session_with_tls(Box::new(CancellableSocket { stream, stopping }), runtime, Some(Arc::new(auth)), tls)
}

/// Outcome of the startup exchange before the session starts.
enum StartupOutcome {
    /// The client sent a protocol-v3 `StartupMessage`.
    Parameters(StartupParameters),
    /// The client sent an `SSLRequest` and a certificate is configured.
    Upgrade(Arc<super::tls::PgTls>),
    /// The client asked to cancel a running query.
    Cancelled,
    /// A fatal startup failure was already rendered for the client.
    Fatal,
}

fn serve_session_with_tls(
    mut stream: Box<dyn SessionStream>,
    runtime: Option<Arc<PgsqlSqlRuntime>>,
    auth: Option<Arc<PgAuthentication>>,
    tls: Option<Arc<super::tls::PgTls>>,
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut session = TransactionSession::new();
    let parameters = match read_startup_outcome(&mut stream, &mut pending, tls.as_ref(), runtime.as_ref())? {
        StartupOutcome::Parameters(parameters) => parameters,
        StartupOutcome::Cancelled | StartupOutcome::Fatal => return Ok(()),
        StartupOutcome::Upgrade(tls) => {
            // `S` promises the TLS handshake; the same session loop then runs over
            // the tunnel, and a client that speaks plaintext into it fails the
            // handshake instead of reaching SQL.
            stream.write_all(b"S")?;
            let mut secured = rustls::StreamOwned::new(tls.server_connection()?, stream);
            return serve_session_with_tls(Box::new(secured), runtime, auth, None);
        }
    };

    session.set_database(parameters.database().to_string());
    session.set_user(parameters.user().to_string());

    if let Some(failure) =
        authenticate(&mut stream, &mut pending, &parameters, auth.as_deref(), runtime.as_ref().map(|runtime| runtime.objects.as_ref()))?
    {
        write_fatal(&mut stream, failure.sqlstate, &failure.message)?;
        return Ok(());
    }

    send_authentication_ok(&mut stream)?;
    send_parameter(&mut stream, "server_version", pg_server_version())?;
    send_parameter(&mut stream, "server_encoding", "UTF8")?;
    send_parameter(&mut stream, "client_encoding", "UTF8")?;
    send_parameter(&mut stream, "DateStyle", "ISO, MDY")?;
    send_parameter(&mut stream, "integer_datetimes", "on")?;
    send_parameter(&mut stream, "standard_conforming_strings", "on")?;
    send_parameter(&mut stream, "TimeZone", "UTC")?;
    if let Some(application_name) = parameters.get("application_name") {
        send_parameter(&mut stream, "application_name", application_name)?;
    }
    let (process_id, secret_key) = send_backend_key(&mut stream)?;
    let _cancel_guard = CancelGuard { runtime: runtime.clone(), process_id, secret_key };
    if let Some(runtime) = runtime.as_ref() {
        let token = runtime.cancels.register(process_id, secret_key);
        session.set_cancel_token(token);
    }
    send_ready(&mut stream, &mut session)?;

    let mut extended = ExtendedSession::default();
    let mut extended_failed = false;

    loop {
        let message = match read_message(&mut stream, &mut pending) {
            Ok(Some(message)) => message,
            Ok(None) => {
                // A connection that goes away mid-transaction is an implicit
                // rollback, catalog changes included.
                let _ = session.restore_schema(runtime.as_ref());
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                let _ = session.restore_schema(runtime.as_ref());
                return Ok(());
            }
            Err(error) => {
                let _ = session.restore_schema(runtime.as_ref());
                return Err(error);
            }
        };
        match message.tag {
            b'X' if message.payload.is_empty() => {
                let _ = session.restore_schema(runtime.as_ref());
                return Ok(());
            }
            b'Q' => {
                let Some(sql_bytes) = message.payload.strip_suffix(&[0])
                else {
                    write_error_response(&mut stream, "08P01", "malformed PostgreSQL simple-query message")?;
                    session.fail();
                    send_ready(&mut stream, &mut session)?;
                    continue;
                };
                if sql_bytes.contains(&0) {
                    write_error_response(&mut stream, "08P01", "embedded NUL in PostgreSQL query")?;
                    session.fail();
                }
                else if let Ok(sql) = std::str::from_utf8(sql_bytes) {
                    if let Ok(Some(utility)) = database_query::parse_pg_utility(sql) {
                        if database_execution::pgsql::CopyInPlan::from_utility(&utility).is_some() {
                            handle_copy_from_stdin(&mut stream, &mut pending, &utility, &mut session, runtime.as_ref())?;
                            send_ready(&mut stream, &mut session)?;
                            continue;
                        }
                    }
                    handle_simple_query(&mut stream, sql, &mut session, &mut extended, runtime.as_ref())?;
                }
                else {
                    write_error_response(&mut stream, "22021", "query is not valid UTF-8")?;
                    session.fail();
                }
                send_ready(&mut stream, &mut session)?;
            }
            tag @ (b'P' | b'B' | b'D' | b'E' | b'C' | b'H' | b'S') => {
                if extended_failed && !matches!(tag, b'S' | b'H') {
                    // 扩展查询出错后，PostgreSQL 会丢弃同一批次的后续消息直到 Sync。
                    continue;
                }
                let step = match tag {
                    b'P' => handle_parse(&mut stream, &message.payload, &mut extended),
                    b'B' => handle_bind(&mut stream, &message.payload, &mut extended, runtime.as_ref()),
                    b'D' => handle_describe(&mut stream, &message.payload, &mut extended, &mut session, runtime.as_ref()),
                    b'E' => handle_execute(&mut stream, &message.payload, &mut extended, &mut session, runtime.as_ref()),
                    b'C' => handle_close(&mut stream, &message.payload, &mut extended),
                    b'H' => stream.flush().map(|_| ExtendedStep::Continue),
                    b'S' => {
                        extended_failed = false;
                        send_ready(&mut stream, &mut session).map(|_| ExtendedStep::Continue)
                    }
                    _ => Ok(ExtendedStep::Continue),
                }?;
                match step {
                    ExtendedStep::Continue => {}
                    ExtendedStep::Failed => {
                        extended_failed = true;
                        session.fail();
                    }
                }
            }
            _ => {
                write_error_response(&mut stream, "08P01", "unsupported PostgreSQL frontend message")?;
                send_ready(&mut stream, &mut session)?;
            }
        }
    }
}

/// Reads startup packets until one of them either starts the session or asks
/// for a TLS upgrade.
///
/// `SSLRequest` is answered `N` when no certificate is configured, which is the
/// documented `psql` fallback, and answered by the caller with `S` when one is.
fn read_startup_outcome(
    stream: &mut (impl Read + Write),
    pending: &mut Vec<u8>,
    tls: Option<&Arc<super::tls::PgTls>>,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<StartupOutcome> {
    loop {
        let packet = read_startup(stream, pending)?;
        match packet.code {
            SSL_REQUEST => match tls {
                Some(tls) => return Ok(StartupOutcome::Upgrade(Arc::clone(tls))),
                None => stream.write_all(b"N")?,
            },
            GSSENC_REQUEST => stream.write_all(b"N")?,
            CANCEL_REQUEST => {
                // `CancelRequest` carries the key pair from `BackendKeyData`.
                // The registry only raises a flag; the target session observes
                // it at its next statement boundary, so a cancel that arrives
                // while a statement runs cannot publish a later write.
                if let Some(runtime) = runtime
                    && packet.payload.len() >= 8
                {
                    let process_id = i32::from_be_bytes([packet.payload[0], packet.payload[1], packet.payload[2], packet.payload[3]]);
                    let secret_key = i32::from_be_bytes([packet.payload[4], packet.payload[5], packet.payload[6], packet.payload[7]]);
                    runtime.cancels.request(process_id, secret_key);
                }
                return Ok(StartupOutcome::Cancelled);
            }
            PROTOCOL_V3 => {
                if tls.is_some_and(|tls| tls.requires_client_tls()) {
                    write_fatal(stream, "28000", "TLS is required: send an `SSLRequest` before the startup packet")?;
                    return Ok(StartupOutcome::Fatal);
                }
                match StartupParameters::parse(&packet.payload) {
                    Ok(parameters) => return Ok(StartupOutcome::Parameters(parameters)),
                    Err(error) => {
                        write_fatal(stream, "08P01", &format_startup_error(error))?;
                        return Ok(StartupOutcome::Fatal);
                    }
                }
            }
            _ => {
                write_fatal(stream, "0A000", "unsupported PostgreSQL protocol version")?;
                return Ok(StartupOutcome::Fatal);
            }
        }
    }
}

/// How a rejected `RESET` / `DISCARD` session command affects the session. Both
/// wire paths (SimpleQuery and the extended protocol) report the same SQLSTATE
/// and abort an open transaction block the same way, so the decision lives here
/// and cannot diverge.
enum SessionCommandError {
    /// The command was refused before it performed any cleanup (no GUC reset,
    /// no prepared-statement or portal drop, no transaction handle change), but
    /// PostgreSQL raises the guard as a plain error, so an open transaction
    /// block aborts exactly like it would for any other statement failure.
    /// This is `DISCARD ALL` inside a transaction block (`25001`).
    Rejected { code: &'static str, message: &'static str },
    /// The command failed against the session; the caller must abort any open
    /// transaction (`25P02` from an aborted block, `42704` for an unknown GUC,
    /// `0A000` for an unsupported `DISCARD` target).
    Failed { code: String, message: String },
}

/// Executes a `RESET <parameter>` / `RESET ALL` / `DISCARD <target>` session
/// command against the session. Shared by SimpleQuery and the extended protocol
/// so both paths run the same side effects ([`TransactionSession::reset_parameters`]
/// and [`TransactionSession::discard_all`]) with the same error mapping.
///
/// `Ok` carries the CommandComplete tag PostgreSQL reports for the statement.
fn run_session_command(
    command: &SqlSessionCommand,
    session: &mut TransactionSession,
    extended: &mut ExtendedSession,
) -> Result<&'static str, SessionCommandError> {
    // `RESET` and `DISCARD` are ordinary utility statements, so an aborted
    // transaction rejects them exactly like any other command (`25P02`).
    if session.is_aborted() {
        return Err(SessionCommandError::Failed {
            code: "25P02".to_string(),
            message: "current transaction is aborted, commands ignored until end of transaction block".to_string(),
        });
    }
    match command {
        SqlSessionCommand::ResetVariables { name } => {
            match name.as_deref() {
                // `RESET ALL` restores every emulated GUC to its default value.
                None => session.reset_parameters(),
                Some(name) => {
                    if pg_parameter(name).is_none() {
                        return Err(SessionCommandError::Failed {
                            code: "42704".to_string(),
                            message: format!("unrecognized configuration parameter \"{name}\""),
                        });
                    }
                    // The default table is the RESET value; a parameter the
                    // session overrode returns to it.
                    session.reset_parameter(name);
                }
            }
            Ok(if name.is_none() { "RESET ALL" } else { "RESET" })
        }
        SqlSessionCommand::Discard(target) => match target {
            // PostgreSQL refuses `DISCARD ALL` inside a transaction block with
            // `25001` and performs no cleanup at all; the raised error still
            // aborts the block like any other statement failure.
            // Outside a block it applies the full session reset connection pools
            // expect: emulated GUCs return to their defaults and the
            // prepared-statement/portal caches are dropped. Temporary tables and
            // advisory locks are outside the gateway subset, so nothing else is
            // released here.
            DiscardTarget::All => {
                if session.in_transaction() {
                    return Err(SessionCommandError::Rejected { code: "25001", message: "DISCARD ALL cannot run inside a transaction block" });
                }
                session.discard_all();
                extended.statements.clear();
                extended.portals.clear();
                Ok("DISCARD ALL")
            }
            // The remaining targets only release caches this gateway does not keep
            // (`PLANS`, `SEQUENCES`) or objects it does not create (`TEMPORARY`).
            // They stay explicitly unsupported instead of silently pretending to
            // perform a cleanup.
            other => {
                let label = match other {
                    DiscardTarget::Plans => "DISCARD PLANS",
                    DiscardTarget::Sequences => "DISCARD SEQUENCES",
                    DiscardTarget::Temporary => "DISCARD TEMPORARY",
                    DiscardTarget::All => unreachable!("`DISCARD ALL` is handled above"),
                };
                Err(SessionCommandError::Failed { code: "0A000".to_string(), message: format!("{label} is not supported by the gateway") })
            }
        },
        _ => unreachable!("only `RESET` and `DISCARD` reach the session command executor"),
    }
}

/// Runs one `COPY ... FROM STDIN` exchange.
///
/// The protocol session owns the frames — `CopyInResponse`, `CopyData`,
/// `CopyDone`, `CopyFail` — while `database.execution::pgsql::copy` owns the
/// data semantics. A failure still answers `ReadyForQuery`, so the connection
/// stays usable exactly like PostgreSQL.
fn handle_copy_from_stdin(
    stream: &mut (impl Read + Write),
    pending: &mut Vec<u8>,
    utility: &database_query::PgUtilityStatement,
    session: &mut TransactionSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<()> {
    let Some(plan) = database_execution::pgsql::CopyInPlan::from_utility(utility)
    else {
        return write_error_response(stream, "08P01", "malformed COPY statement");
    };
    let Some(runtime) = runtime
    else {
        return write_error_response(stream, "0A000", "PostgreSQL query execution is not implemented (no SQL runtime)");
    };
    let column_count = match copy_column_count(&plan, runtime) {
        Ok(count) => count,
        Err((code, message)) => return write_error_response(stream, &code, &message),
    };
    // CopyInResponse: overall format 0 (text), then one format code per column.
    let mut payload = Vec::with_capacity(3 + column_count * 2);
    payload.push(0);
    payload.extend_from_slice(&(column_count as i16).to_be_bytes());
    for _ in 0..column_count {
        payload.extend_from_slice(&0_i16.to_be_bytes());
    }
    write_message(stream, b'G', &payload)?;
    let mut state = database_execution::pgsql::CopyInState::new(plan);
    loop {
        let Some(message) = read_message(stream, pending)?
        else {
            return Ok(());
        };
        match message.tag {
            b'd' => {
                state.push(&message.payload);
                if state.len() > MAX_COPY_BYTES {
                    return write_error_response(stream, "54000", "COPY FROM STDIN exceeded the gateway buffer limit");
                }
            }
            b'c' => {
                return match state.finish(runtime, session) {
                    Ok(inserted) => write_command_complete(stream, &format!("COPY {inserted}")),
                    Err(error) => write_error_response(stream, &error.sqlstate, &error.message),
                };
            }
            b'f' => {
                let detail = cstring(&message.payload).unwrap_or_default();
                session.fail();
                return write_error_response(stream, "57014", &format!("COPY from stdin failed: {detail}"));
            }
            _ => {
                session.fail();
                return write_error_response(stream, "08P01", "unexpected frontend message during COPY FROM STDIN");
            }
        }
    }
}

/// Column count reported in `CopyInResponse`, resolved through the catalog when
/// `COPY` did not name the columns.
fn copy_column_count(plan: &database_execution::pgsql::CopyInPlan, runtime: &Arc<PgsqlSqlRuntime>) -> Result<usize, (String, String)> {
    if !plan.columns.is_empty() {
        return Ok(plan.columns.len());
    }
    let catalog = runtime.schema.catalog();
    catalog
        .tables()
        .iter()
        .find(|table| table.name == plan.table)
        .map(|table| table.fields.len())
        .ok_or_else(|| ("42P01".to_string(), format!("relation \"{}\" does not exist", plan.table)))
}

fn handle_simple_query(
    stream: &mut impl Write,
    sql: &str,
    session: &mut TransactionSession,
    extended: &mut ExtendedSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<()> {
    // PostgreSQL answers an empty query string with EmptyQueryResponse rather
    // than a syntax error. Connection pools (`r2d2` and others) probe a
    // checked-out connection exactly this way, so an error here rejects every
    // checkout even though the connection itself is healthy.
    if sql.trim().is_empty() {
        return write_message(stream, b'I', &[]);
    }
    let statements = match split_sql_batch(sql) {
        Ok(statements) => statements,
        Err(error) => {
            session.fail();
            return write_error_response(stream, pg_sqlstate(error.kind), &format!("SQL batch error: {}", error.message));
        }
    };
    if statements.len() <= 1 {
        let single = statements.first().map(|statement| statement.text.as_str()).unwrap_or(sql);
        return handle_single_statement(stream, single, session, extended, runtime);
    }
    // PostgreSQL parses the whole string and stops at the first failing
    // statement. Outside a transaction block each statement is its own implicit
    // transaction, so earlier statements stay committed. Inside a block the
    // checkpoint captured at `BEGIN` owns the schema rollback, so a failing
    // statement only aborts the block — `ROLLBACK`, `DISCARD ALL` or a
    // disconnect restores the catalog.
    for (index, statement) in statements.iter().enumerate() {
        let mut buffer = Vec::new();
        handle_single_statement(&mut buffer, &statement.text, session, extended, runtime)?;
        match annotate_batch_error(&buffer, index + 1, statements.len(), statement.offset) {
            Some(annotated) => {
                stream.write_all(&annotated)?;
                return Ok(());
            }
            None => stream.write_all(&buffer)?,
        }
    }
    Ok(())
}

/// Executes one already-split statement through the shared execution dispatch.
///
/// `database.execution::pgsql` owns the DDL, transaction and bind → plan →
/// execute order, so the local engine and this wire path cannot drift. This
/// function only maps the outcome onto protocol messages; the session-command
/// surfaces that own prepared-statement caches stay here.
fn handle_single_statement(
    stream: &mut impl Write,
    sql: &str,
    session: &mut TransactionSession,
    extended: &mut ExtendedSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<()> {
    match dispatch_statement(sql, runtime, session) {
        Ok(StatementOutcome::Command(tag)) => write_command_complete(stream, tag),
        Ok(StatementOutcome::Result(outcome)) => write_execution_outcome(stream, &outcome),
        Ok(StatementOutcome::Session(command)) => handle_session_command(stream, command, sql, session, extended, runtime),
        Err(error) => write_error_response(stream, &error.sqlstate, &error.message),
    }
}

/// Session commands the extended protocol executes itself.
///
/// `SET` / `SHOW` / `RESET` / `DISCARD` mutate session state and belong to the
/// protocol surface; transaction control, savepoints and `SET NAMES` keep their
/// existing extended-protocol path, so they are not claimed here.
fn extended_session_command(sql: &str) -> Option<SqlSessionCommand> {
    let normalized = normalized_session_syntax(sql);
    let command = parse_session_command(normalized.as_deref().unwrap_or(sql)).ok().flatten()?;
    matches!(
        command,
        SqlSessionCommand::SetVariable { .. }
            | SqlSessionCommand::ShowParameter { .. }
            | SqlSessionCommand::ResetVariables { .. }
            | SqlSessionCommand::Discard(_)
    )
    .then_some(command)
}

/// One described `SHOW <parameter>` result column.
///
/// PostgreSQL reports the parameter name as the column label and `text` as the
/// type, on both the simple and the extended path.
fn show_column(name: &str) -> SqlColumn {
    SqlColumn { name: name.to_string(), field_type: SqlFieldType::String, field_id: None }
}

/// Applies one `SET <name> = <value>` to the session.
///
/// `search_path` is the only parameter the gateway interprets; every other known
/// parameter keeps the written value for the session, and an unknown name reports
/// `42704`.
fn apply_set_variable(session: &mut TransactionSession, name: &str, value: &SqlBoundExpression) -> Result<(), (String, String)> {
    let needle = name.trim().to_ascii_lowercase();
    if needle == "search_path" {
        return match value {
            SqlBoundExpression::Literal(SqlBoundLiteral::String(path)) => {
                session.set_search_path(path.clone());
                Ok(())
            }
            // `SET search_path TO public` writes an unquoted identifier, which is
            // the spelling clients use most; it means the same schema list.
            SqlBoundExpression::Identifier(path) => {
                session.set_search_path(path.clone());
                Ok(())
            }
            other => {
                // The engine rejects any non-string value with `22023
                // invalid_parameter_value` and echoes the value it saw, so the
                // error codes and text stay comparable.
                let rendered = match other {
                    SqlBoundExpression::Literal(SqlBoundLiteral::Integer(number)) => number.to_string(),
                    SqlBoundExpression::Literal(SqlBoundLiteral::Number(number)) => number.clone(),
                    SqlBoundExpression::Literal(SqlBoundLiteral::Boolean(flag)) => flag.to_string(),
                    SqlBoundExpression::Identifier(name) => name.clone(),
                    SqlBoundExpression::Literal(SqlBoundLiteral::Null) => "NULL".to_string(),
                    SqlBoundExpression::Literal(SqlBoundLiteral::Bytes(_)) => "\\x".to_string(),
                    _ => String::new(),
                };
                Err(("22023".to_string(), format!("invalid value for parameter \"search_path\": \"{rendered}\"")))
            }
        };
    }
    if needle == "statement_timeout" {
        let rendered = match value {
            SqlBoundExpression::Literal(SqlBoundLiteral::Integer(number)) => number.to_string(),
            SqlBoundExpression::Literal(SqlBoundLiteral::Number(number)) => number.clone(),
            SqlBoundExpression::Literal(SqlBoundLiteral::String(text)) => text.clone(),
            SqlBoundExpression::Identifier(text) => text.clone(),
            _ => String::new(),
        };
        if session.set_statement_timeout(&rendered) {
            return Ok(());
        }
        return Err(("22023".to_string(), format!("invalid value for parameter \"statement_timeout\": \"{rendered}\"")));
    }
    if pg_parameter(name).is_none() {
        return Err(("42704".to_string(), format!("unrecognized configuration parameter \"{name}\"")));
    }
    // PostgreSQL keeps the written value for the session; storing it is what
    // makes `SHOW <name>` and `current_setting('<name>')` report it back.
    let rendered = match value {
        SqlBoundExpression::Literal(SqlBoundLiteral::String(text)) => text.clone(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Integer(number)) => number.to_string(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Number(number)) => number.clone(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Boolean(flag)) => flag.to_string(),
        SqlBoundExpression::Identifier(text) => text.clone(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Null) => "NULL".to_string(),
        _ => String::new(),
    };
    session.set_parameter(name, rendered);
    Ok(())
}

/// Value of one `SHOW <parameter>`.
///
/// The session knows the mutable parameters; `server_version` stays with the
/// server crate's banner, so it resolves through the same lookup.
fn show_parameter(session: &TransactionSession, name: &str) -> Result<String, (String, String)> {
    session
        .parameter(name)
        .or_else(|| pg_parameter(name).map(str::to_string))
        .ok_or_else(|| ("42704".to_string(), format!("unrecognized configuration parameter \"{name}\"")))
}

/// Applies one session command on the extended protocol.
///
/// `SHOW` is the only variant that produces a row; the caller owns the
/// `DataRow` framing because it also owns the portal's result formats.
fn apply_extended_session_command(
    command: &SqlSessionCommand,
    session: &mut TransactionSession,
    extended: &mut ExtendedSession,
) -> Result<SessionCommandOutcome, SessionCommandError> {
    if session.is_aborted() {
        return Err(SessionCommandError::Failed { code: "25P02".to_string(), message: ABORTED_BLOCK.to_string() });
    }
    match command {
        SqlSessionCommand::SetVariable { name, value, .. } => match apply_set_variable(session, name, value) {
            Ok(()) => Ok(SessionCommandOutcome::Command("SET")),
            Err((code, message)) => Err(SessionCommandError::Failed { code, message }),
        },
        SqlSessionCommand::ShowParameter { name, .. } => match show_parameter(session, name) {
            Ok(value) => Ok(SessionCommandOutcome::Row { name: name.clone(), value }),
            Err((code, message)) => Err(SessionCommandError::Failed { code, message }),
        },
        SqlSessionCommand::ShowVariables { .. } => Err(SessionCommandError::Failed {
            code: "42601".to_string(),
            message: "SHOW VARIABLES is MySQL syntax, PostgreSQL uses SHOW <parameter>".to_string(),
        }),
        SqlSessionCommand::Pragma { .. } => {
            Err(SessionCommandError::Failed { code: "42601".to_string(), message: "PRAGMA is not PostgreSQL syntax".to_string() })
        }
        command @ (SqlSessionCommand::ResetVariables { .. } | SqlSessionCommand::Discard(_)) => {
            run_session_command(command, session, extended).map(SessionCommandOutcome::Command)
        }
        SqlSessionCommand::Savepoint(_) | SqlSessionCommand::SetNames { .. } | SqlSessionCommand::Transaction(_) => {
            Err(SessionCommandError::Failed {
                code: "0A000".to_string(),
                message: "session command is not available on the extended protocol".to_string(),
            })
        }
    }
}

/// Outcome of one extended-protocol session command.
enum SessionCommandOutcome {
    /// Completed with a command tag and no rows.
    Command(&'static str),
    /// `SHOW` answered one row of text.
    Row {
        /// Column label, the parameter name as written.
        name: String,
        /// Parameter value.
        value: String,
    },
}

/// Wire-side session commands: `SET` / `SHOW`, `RESET`, `DISCARD` and
/// savepoints. They mutate session GUCs and prepared-statement caches, which
/// belong to the protocol surface, so dispatch hands them back unapplied.
fn handle_session_command(
    stream: &mut impl Write,
    command: SqlSessionCommand,
    sql: &str,
    session: &mut TransactionSession,
    extended: &mut ExtendedSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<()> {
    match command {
        SqlSessionCommand::Savepoint(intent) => {
            use database_query::SavepointIntent;
            if session.is_aborted() {
                return write_error_response(stream, "25P02", ABORTED_BLOCK);
            }
            let Some(handle) = session.transaction_handle()
            else {
                session.fail();
                return write_error_response(stream, "25P01", "SAVEPOINT can only be used in transaction blocks");
            };
            let result = match &intent {
                SavepointIntent::Create { name } => handle.savepoint_create(name),
                SavepointIntent::RollbackTo { name } => handle.savepoint_rollback_to(name),
                SavepointIntent::Release { name } => handle.savepoint_release(name),
            };
            match result {
                Ok(()) => {
                    let tag = match intent {
                        SavepointIntent::Create { .. } => "SAVEPOINT",
                        SavepointIntent::RollbackTo { .. } => "ROLLBACK",
                        SavepointIntent::Release { .. } => "RELEASE",
                    };
                    write_command_complete(stream, tag)
                }
                Err(database_types::cluster::Error::Unsupported(msg)) if msg.contains("does not exist") || msg.contains("already exists") => {
                    session.fail();
                    write_error_response(stream, "3B000", msg)
                }
                Err(err) => {
                    session.fail();
                    write_error_response(stream, "58P01", &format!("savepoint failed: {err}"))
                }
            }
        }
        SqlSessionCommand::SetVariable { name, value, .. } => {
            if session.is_aborted() {
                return write_error_response(stream, "25P02", ABORTED_BLOCK);
            }
            match apply_set_variable(session, &name, &value) {
                Ok(()) => write_command_complete(stream, "SET"),
                Err((code, message)) => {
                    session.fail();
                    write_error_response(stream, &code, &message)
                }
            }
        }
        SqlSessionCommand::ShowParameter { name, .. } => {
            if session.is_aborted() {
                return write_error_response(stream, "25P02", ABORTED_BLOCK);
            }
            match show_parameter(session, &name) {
                Ok(value) => {
                    write_row_description(stream, &[show_column(&name)], &[0])?;
                    write_data_row(stream, &[SqlValue::String(value)], &[show_column(&name)], &[0])?;
                    write_command_complete(stream, "SHOW")
                }
                Err((code, message)) => {
                    session.fail();
                    write_error_response(stream, &code, &message)
                }
            }
        }
        SqlSessionCommand::ShowVariables { .. } => {
            session.fail();
            write_error_response(stream, "42601", "SHOW VARIABLES is MySQL syntax, PostgreSQL uses SHOW <parameter>")
        }
        SqlSessionCommand::Pragma { .. } => {
            session.fail();
            write_error_response(stream, "42601", "PRAGMA is not PostgreSQL syntax")
        }
        command @ (SqlSessionCommand::ResetVariables { .. } | SqlSessionCommand::Discard(_)) => {
            match run_session_command(&command, session, extended) {
                Ok(tag) => write_command_complete(stream, tag),
                Err(SessionCommandError::Rejected { code, message }) => {
                    session.fail();
                    write_error_response(stream, code, message)
                }
                Err(SessionCommandError::Failed { code, message }) => {
                    session.fail();
                    write_error_response(stream, &code, &message)
                }
            }
        }
        SqlSessionCommand::SetNames { .. } => {
            if session.is_aborted() {
                return write_error_response(stream, "25P02", ABORTED_BLOCK);
            }
            match runtime {
                Some(rt) => execute_simple_query(stream, sql, rt, session),
                None => {
                    session.fail();
                    write_error_response(stream, "0A000", "PostgreSQL query execution is not implemented (no SQL runtime)")
                }
            }
        }
        SqlSessionCommand::Transaction(_) => {
            unreachable!("transaction control is dispatched by `database.execution::pgsql`")
        }
    }
}

/// SimpleQuery 的 SQL bind → plan → execute 路径。
fn execute_simple_query(
    stream: &mut impl Write,
    sql: &str,
    runtime: &Arc<PgsqlSqlRuntime>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    match run_statement(sql, runtime, session) {
        Ok(outcome) => write_execution_outcome(stream, &outcome),
        Err((code, message)) => write_error_response(stream, &code, &message),
    }
}

fn write_execution_outcome(stream: &mut impl Write, outcome: &ExecutionOutcome) -> io::Result<()> {
    if outcome.returns_rows() {
        let formats = vec![0_i16; outcome.description.len()];
        write_row_description(stream, &outcome.description, &formats)?;
        for row in &outcome.rows {
            write_data_row(stream, row, &outcome.description, &formats)?;
        }
    }
    write_command_complete(stream, &outcome.command_tag)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtendedStep {
    Continue,
    /// The error suspends the message queue until Sync and aborts the session
    /// transaction, matching PostgreSQL for every extended-protocol failure,
    /// including the `DISCARD ALL` transaction-block guard (`25001`).
    Failed,
}

/// 扩展查询协议的会话状态：命名/匿名 statement 与 portal。
#[derive(Default)]
struct ExtendedSession {
    statements: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
}

struct PreparedStatement {
    sql: String,
    parameter_oids: Vec<u32>,
    /// Catalog-derived result shape, computed once on the first `Describe(S)`
    /// or `Bind`. Writes describe to an empty shape.
    description: Option<Vec<SqlColumn>>,
    /// Session utility command (`RESET` / `DISCARD`) recognised by the shared
    /// lexical pre-filter at Parse time. Utility statements have no catalog
    /// shape: they describe as `NoData` and execute through
    /// [`run_session_command`] instead of the bind → plan → execute path.
    session_command: Option<SqlSessionCommand>,
}

struct Portal {
    /// Statement name this portal was bound from, used to cascade-close portals
    /// when the parent statement is removed (PostgreSQL protocol contract).
    statement_name: String,
    sql: String,
    parameters: Vec<SqlValue>,
    description: Vec<SqlColumn>,
    /// Requested result formats: empty means all text, one entry applies to all
    /// columns, otherwise one entry per described column.
    result_formats: Vec<i16>,
    /// Copied from the bound statement: utility statements execute as session
    /// side effects with no rows.
    session_command: Option<SqlSessionCommand>,
    outcome: Option<Result<ExecutionOutcome, (String, String)>>,
    sent_rows: usize,
}

fn handle_parse(stream: &mut impl Write, payload: &[u8], extended: &mut ExtendedSession) -> io::Result<ExtendedStep> {
    let mut cursor = 0;
    let (Some(name), Some(sql)) = (take_cstring(payload, &mut cursor), take_cstring(payload, &mut cursor))
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Parse message");
    };
    let Some(parameter_count) = take_i16(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Parse message");
    };
    if parameter_count < 0 {
        return extended_protocol_error(stream, "08P01", "negative PostgreSQL parameter count");
    }
    let mut parameter_oids = Vec::with_capacity(parameter_count as usize);
    for _ in 0..parameter_count {
        let Some(oid) = take_u32(payload, &mut cursor)
        else {
            return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Parse parameter types");
        };
        parameter_oids.push(oid);
    }
    // The extended protocol recognises the same session commands as SimpleQuery
    // through the one shared parser, so a `SET` / `SHOW` / `RESET` / `DISCARD`
    // Parse is stored as a session command instead of being deferred to the
    // catalog binder, which would reject it as an unsupported statement.
    let session_command = extended_session_command(sql);
    let description = match &session_command {
        // `SHOW <parameter>` reports one `text` column named after the parameter.
        Some(SqlSessionCommand::ShowParameter { name, .. }) => Some(vec![show_column(name)]),
        Some(_) => Some(Vec::new()),
        None => None,
    };
    extended
        .statements
        .insert(name.to_string(), PreparedStatement { sql: sql.to_string(), parameter_oids, description, session_command });
    write_message(stream, b'1', &[])?;
    Ok(ExtendedStep::Continue)
}

fn handle_bind(
    stream: &mut impl Write,
    payload: &[u8],
    extended: &mut ExtendedSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<ExtendedStep> {
    let mut cursor = 0;
    let (Some(portal_name), Some(statement_name)) = (take_cstring(payload, &mut cursor), take_cstring(payload, &mut cursor))
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind message");
    };
    let Some(statement) = extended.statements.get_mut(statement_name)
    else {
        return extended_protocol_error(stream, "26000", &format!("prepared statement \"{statement_name}\" does not exist"));
    };
    let sql = statement.sql.clone();
    // Bind decodes binary parameter bytes by OID, so it must use the same
    // catalog-inferred types Describe reported rather than only what Parse
    // declared (which is usually empty).
    let parameter_oids = statement_parameter_description(&sql, &statement.parameter_oids, runtime);

    let Some(format_count) = take_i16(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind message");
    };
    let mut parameter_formats = Vec::with_capacity(format_count.max(0) as usize);
    for _ in 0..format_count.max(0) {
        let Some(format) = take_i16(payload, &mut cursor)
        else {
            return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind format codes");
        };
        if format != 0 && format != 1 {
            return extended_protocol_error(stream, "08P01", &format!("unsupported PostgreSQL format code {format}"));
        }
        parameter_formats.push(format);
    }

    let Some(parameter_count) = take_i16(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind message");
    };
    if parameter_formats.len() > 1 && parameter_formats.len() != parameter_count.max(0) as usize {
        return extended_protocol_error(stream, "08P01", "PostgreSQL parameter format count does not match parameters");
    }
    let mut parameters = Vec::with_capacity(parameter_count.max(0) as usize);
    for index in 0..parameter_count.max(0) {
        let Some(length) = take_i32(payload, &mut cursor)
        else {
            return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind parameter length");
        };
        if length < 0 {
            parameters.push(SqlValue::Null);
            continue;
        }
        let Some(bytes) = take_bytes(payload, &mut cursor, length as usize)
        else {
            return extended_protocol_error(stream, "08P01", "truncated PostgreSQL Bind parameter");
        };
        let oid = parameter_oids.get(index as usize).copied().unwrap_or(0);
        let format = match parameter_formats.len() {
            0 => 0,
            1 => parameter_formats[0],
            _ => parameter_formats[index as usize],
        };
        let decoded = if format == 1 { binary_parameter_value(oid, bytes) } else { parameter_value(oid, bytes) };
        match decoded {
            Ok(value) => parameters.push(value),
            Err((code, message)) => return extended_protocol_error(stream, &code, &message),
        }
    }

    let Some(result_format_count) = take_i16(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind result formats");
    };
    let mut result_formats = Vec::with_capacity(result_format_count.max(0) as usize);
    for _ in 0..result_format_count.max(0) {
        let Some(format) = take_i16(payload, &mut cursor)
        else {
            return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Bind result formats");
        };
        if format != 0 && format != 1 {
            return extended_protocol_error(stream, "08P01", &format!("unsupported PostgreSQL format code {format}"));
        }
        result_formats.push(format);
    }

    let session_command = statement.session_command.clone();
    if statement.description.is_none() {
        if session_command.is_some() {
            // Utility statements have no catalog result shape; they describe as
            // `NoData` exactly like PostgreSQL utility statements do.
            statement.description = Some(Vec::new());
        }
        else {
            match statement_description(&sql, runtime, Some(&parameters)) {
                Ok(description) => statement.description = Some(description),
                Err((code, message)) => return extended_protocol_error(stream, &code, &message),
            }
        }
    }
    let description = statement.description.clone().unwrap_or_default();
    if result_formats.len() > 1 && result_formats.len() != description.len() {
        return extended_protocol_error(stream, "08P01", "PostgreSQL result format count does not match columns");
    }

    extended.portals.insert(
        portal_name.to_string(),
        Portal {
            statement_name: statement_name.to_string(),
            sql,
            parameters,
            description,
            result_formats,
            session_command,
            outcome: None,
            sent_rows: 0,
        },
    );
    write_message(stream, b'2', &[])?;
    Ok(ExtendedStep::Continue)
}

fn handle_describe(
    stream: &mut impl Write,
    payload: &[u8],
    extended: &mut ExtendedSession,
    session: &mut TransactionSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<ExtendedStep> {
    let Some((&kind, rest)) = payload.split_first()
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Describe message");
    };
    let mut cursor = 0;
    let Some(name) = take_cstring(rest, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Describe target");
    };
    match kind {
        b'S' => {
            let Some(statement) = extended.statements.get_mut(name)
            else {
                return extended_protocol_error(stream, "26000", &format!("prepared statement \"{name}\" does not exist"));
            };
            let parameters = statement_parameter_description(&statement.sql, &statement.parameter_oids, runtime);
            // Session utility statements have no catalog shape; the empty
            // description below answers them with `NoData`, like PostgreSQL.
            if statement.description.is_none() && statement.session_command.is_none() {
                match statement_description(&statement.sql, runtime, None) {
                    Ok(description) => statement.description = Some(description),
                    Err((code, message)) => {
                        write_error_response(stream, &code, &message)?;
                        return Ok(ExtendedStep::Failed);
                    }
                }
            }
            write_parameter_description(stream, &parameters)?;
            match statement.description.as_deref() {
                Some([]) | None => write_message(stream, b'n', &[])?,
                Some(description) => {
                    let formats = vec![0_i16; description.len()];
                    write_row_description(stream, description, &formats)?;
                }
            }
            Ok(ExtendedStep::Continue)
        }
        b'P' => {
            // Describe(portal) reports shape only. Execution belongs to Execute.
            let Some(portal) = extended.portals.get(name)
            else {
                return extended_protocol_error(stream, "34000", &format!("portal \"{name}\" does not exist"));
            };
            if session.is_aborted() {
                write_error_response(stream, "25P02", "current transaction is aborted, commands ignored until end of transaction block")?;
                return Ok(ExtendedStep::Failed);
            }
            if portal.description.is_empty() {
                write_message(stream, b'n', &[])?;
            }
            else {
                let formats = resolve_formats(&portal.result_formats, portal.description.len());
                write_row_description(stream, &portal.description, &formats)?;
            }
            Ok(ExtendedStep::Continue)
        }
        _ => extended_protocol_error(stream, "08P01", "unsupported PostgreSQL Describe target"),
    }
}

/// Number of placeholders the SQL surface references, padding declared
/// parameter OIDs with zero (`unspecified`) when the client sent fewer.
///
/// An `unspecified` OID forces strict drivers (`tokio-postgres` and the sync
/// `postgres` crate) to resolve the type through `pg_catalog`, which this
/// gateway answers with metadata-only shapes. A client that parsed with no
/// OIDs therefore needs the catalog-derived OID here, otherwise every
/// parameterized statement fails before it executes.
fn statement_parameter_description(sql: &str, declared: &[u32], runtime: Option<&Arc<PgsqlSqlRuntime>>) -> Vec<u32> {
    let referenced = neutralized_sql_parameters(sql, SqlParameterStyle::DollarNumber).map(|(_, count)| count).unwrap_or(0);
    let mut parameters = declared.to_vec();
    parameters.resize(parameters.len().max(referenced), 0);
    if let Some(runtime) = runtime {
        let catalog = runtime.schema.catalog();
        let inferred = infer_sql_parameter_types(sql, SqlParameterStyle::DollarNumber, &catalog);
        for (position, oid) in parameters.iter_mut().enumerate() {
            if *oid == 0
                && let Some(Some(field_type)) = inferred.get(position)
            {
                // Parameter slots are zero-based: the parameter at `position`
                // reads inference slot `position`.
                *oid = pg_oid(&field_type.inner());
            }
        }
    }
    for oid in parameters.iter_mut() {
        // PostgreSQL always resolves a parameter to a concrete type. A shape the
        // text scan cannot attribute — `strpos("body", $1)`, for example — falls
        // back to `text`, which is how the gateway's own literal binder reads an
        // untyped value. Returning `unspecified` would make strict drivers query
        // `pg_catalog` for the type.
        if *oid == 0 {
            *oid = OID_TEXT;
        }
    }
    parameters
}

/// Resolves Bind result formats into one entry per described column.
fn resolve_formats(formats: &[i16], columns: usize) -> Vec<i16> {
    match formats.len() {
        0 => vec![0; columns],
        1 => vec![formats[0]; columns],
        _ => formats.to_vec(),
    }
}

/// Derives the statement result shape from the catalog without executing.
///
/// Bound parameters are rendered exactly when they are available, so a
/// describe after Bind cannot disagree with the execution. Metadata-only
/// describes neutralize each placeholder to `0` because a numeric literal
/// keeps pk lookups plannable, and retry with `NULL` when `0` is not writable
/// for the target column.
fn statement_description(
    sql: &str,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    parameters: Option<&[SqlValue]>,
) -> Result<Vec<SqlColumn>, (String, String)> {
    let Some(runtime) = runtime
    else {
        return Err(("0A000".to_string(), "PostgreSQL query execution is not implemented (no SQL runtime)".to_string()));
    };
    // Lower `expr::type` before the metadata and binder stages, exactly like the
    // execution path does.
    let normalized = rewrite_double_colon_casts(sql, SqlDialect::Postgres).map_err(|error| statement_error("cast", error))?;
    let sql = normalized.as_ref();
    if let Some(parameters) = parameters {
        let rendered =
            substitute_sql_parameters(sql, parameters, SqlParameterStyle::DollarNumber).map_err(|error| statement_error("parameter", error))?;
        if let Some(meta) = analyze_meta_query(&rendered, SqlDialect::Postgres).map_err(|error| statement_error("metadata", error))? {
            return Ok(meta.describe());
        }
        return plan_description(&rendered, runtime);
    }
    let (neutral_sql, _) =
        neutralized_sql_parameters(sql, SqlParameterStyle::DollarNumber).map_err(|error| statement_error("parameter", error))?;
    if let Some(meta) = analyze_meta_query(&neutral_sql, SqlDialect::Postgres).map_err(|error| statement_error("metadata", error))? {
        return Ok(meta.describe());
    }
    match describe_without_values(&neutral_sql, runtime) {
        Ok(columns) => Ok(columns),
        Err(first) => {
            // `0` 站不住脚的列（uuid / 文本 / 时间）和主键谓词上的 `NULL` 都会让
            // describe 失败；元数据阶段只求结果形状，按目录推断的参数类型给出
            // 类型正确的站位值再试一次。
            describe_with_typed_stand_ins(sql, runtime).map_err(|_| first)
        }
    }
}

/// Describes a parameterized statement by substituting one type-correct
/// stand-in per inferred parameter type.
///
/// Metadata describes have no client values, and a stand-in that ignores the
/// column type (`0` or `NULL`) is rejected by uuid/text/time columns and by
/// primary-key predicates. The catalog already knows each slot's type, so the
/// stand-in respects it and the plan only serves to expose the result shape.
fn describe_with_typed_stand_ins(sql: &str, runtime: &Arc<PgsqlSqlRuntime>) -> Result<Vec<SqlColumn>, (String, String)> {
    let (_, count) =
        neutralized_sql_parameters(sql, SqlParameterStyle::DollarNumber).map_err(|error| statement_error("parameter", error))?;
    let inferred = infer_sql_parameter_types(sql, SqlParameterStyle::DollarNumber, &runtime.schema.catalog());
    let mut values = Vec::with_capacity(count);
    for slot in 0..count {
        let field_type = inferred.get(slot).cloned().flatten().map(|field_type| field_type.inner());
        values.push(test_stand_in(field_type));
    }
    let rendered =
        substitute_sql_parameters(sql, &values, SqlParameterStyle::DollarNumber).map_err(|error| statement_error("parameter", error))?;
    describe_without_values(&rendered, runtime)
}

/// One value every column type accepts, chosen from the inferred slot type.
fn test_stand_in(field_type: Option<SqlFieldType>) -> SqlValue {
    match field_type {
        Some(SqlFieldType::Uuid) => SqlValue::String("00000000-0000-0000-0000-000000000000".to_owned()),
        Some(SqlFieldType::Bool) => SqlValue::Boolean(false),
        Some(SqlFieldType::Int16 | SqlFieldType::Int32 | SqlFieldType::Int64) => SqlValue::Integer(0),
        Some(SqlFieldType::Float64 | SqlFieldType::Decimal) => SqlValue::Integer(0),
        Some(SqlFieldType::Date) => SqlValue::String("2000-01-01".to_owned()),
        Some(SqlFieldType::Datetime) => SqlValue::String("2000-01-01 00:00:00".to_owned()),
        Some(SqlFieldType::Json) => SqlValue::String("{}".to_owned()),
        Some(SqlFieldType::Bytes) => SqlValue::Bytes(vec![0]),
        Some(SqlFieldType::String) | Some(SqlFieldType::Unknown) | None => SqlValue::String(String::new()),
        Some(SqlFieldType::Optional(_)) => SqlValue::String(String::new()),
        Some(SqlFieldType::Array(_)) => SqlValue::Array(Vec::new()),
    }
}

/// Describes a statement that still carries placeholder stand-ins.
///
/// Write values are validated only when the statement executes, and a
/// metadata describe has no real parameters: a neutral `0` cannot be written
/// to text/uuid/bool columns and a neutral `NULL` cannot be a primary key.
/// Writes therefore describe from their bound shape instead of being planned.
fn describe_without_values(sql: &str, runtime: &Arc<PgsqlSqlRuntime>) -> Result<Vec<SqlColumn>, (String, String)> {
    plan_description(sql, runtime)
}

/// Binds a statement only to read its result shape.
///
/// An `INSERT` never plans here. Sequence-owning columns are filled at execute
/// time, so planning the describe would either fail on the missing primary key
/// or, if the registry allocation were reused, consume `nextval` values the
/// client never observes. `describe_bound_insert` derives the `RETURNING`
/// shape from the bound columns instead.
fn plan_description(sql: &str, runtime: &Arc<PgsqlSqlRuntime>) -> Result<Vec<SqlColumn>, (String, String)> {
    let catalog = runtime.schema.catalog();
    let bound =
        bind_sql_catalog(sql, &catalog, catalog.schema_version(), SqlDialect::Postgres).map_err(|error| statement_error("bind", error))?;
    match bound {
        SqlCatalogBoundStatement::Insert(insert) => Ok(describe_bound_insert(&insert, &catalog, SqlDialect::Postgres)),
        // `UPDATE` / `DELETE` in this gateway carry no `RETURNING` shape.
        SqlCatalogBoundStatement::Update(_) | SqlCatalogBoundStatement::Delete(_) => Ok(Vec::new()),
        bound => {
            let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Postgres).map_err(|error| statement_error("plan", error))?;
            Ok(describe_plan(&plan))
        }
    }
}

fn handle_execute(
    stream: &mut impl Write,
    payload: &[u8],
    extended: &mut ExtendedSession,
    session: &mut TransactionSession,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
) -> io::Result<ExtendedStep> {
    let mut cursor = 0;
    let Some(name) = take_cstring(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Execute message");
    };
    let Some(max_rows) = take_i32(payload, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Execute row limit");
    };
    // Session commands run the same shared side effects as SimpleQuery and
    // answer with `CommandComplete`; `SHOW` additionally emits its one text row.
    let session_command = extended.portals.get(name).and_then(|portal| portal.session_command.clone());
    if let Some(command) = session_command {
        let formats = extended.portals.get(name).map(|portal| resolve_formats(&portal.result_formats, 1)).unwrap_or_else(|| vec![0]);
        return match apply_extended_session_command(&command, session, extended) {
            Ok(SessionCommandOutcome::Command(tag)) => {
                write_command_complete(stream, tag)?;
                Ok(ExtendedStep::Continue)
            }
            Ok(SessionCommandOutcome::Row { name, value }) => {
                let column = show_column(&name);
                write_data_row(stream, &[SqlValue::String(value)], &[column], &formats)?;
                write_command_complete(stream, "SHOW")?;
                Ok(ExtendedStep::Continue)
            }
            Err(SessionCommandError::Rejected { code, message }) => {
                write_error_response(stream, code, message)?;
                Ok(ExtendedStep::Failed)
            }
            Err(SessionCommandError::Failed { code, message }) => {
                write_error_response(stream, &code, &message)?;
                Ok(ExtendedStep::Failed)
            }
        };
    }
    let Some(portal) = extended.portals.get_mut(name)
    else {
        return extended_protocol_error(stream, "34000", &format!("portal \"{name}\" does not exist"));
    };
    // Same contract as SimpleQuery: an empty statement executes to an
    // EmptyQueryResponse with no command tag.
    if portal.sql.trim().is_empty() {
        write_message(stream, b'I', &[])?;
        return Ok(ExtendedStep::Continue);
    }
    prepare_portal(portal, session, runtime);
    let Some(outcome) = portal.outcome.as_ref()
    else {
        unreachable!("prepare_portal always stores an outcome");
    };
    match outcome {
        Ok(outcome) => {
            let command_tag = outcome.command_tag.clone();
            if outcome.returns_rows() {
                let start = portal.sent_rows.min(outcome.rows.len());
                let end = if max_rows > 0 { (start + max_rows as usize).min(outcome.rows.len()) } else { outcome.rows.len() };
                let formats = resolve_formats(&portal.result_formats, outcome.description.len());
                let description = outcome.description.clone();
                let chunk: Vec<Vec<SqlValue>> = outcome.rows[start..end].to_vec();
                portal.sent_rows = end;
                for row in &chunk {
                    write_data_row(stream, row, &description, &formats)?;
                }
                if end < outcome.rows.len() {
                    write_message(stream, b's', &[])?;
                    return Ok(ExtendedStep::Continue);
                }
            }
            write_command_complete(stream, &command_tag)?;
            Ok(ExtendedStep::Continue)
        }
        Err((code, message)) => {
            write_error_response(stream, code, message)?;
            Ok(ExtendedStep::Failed)
        }
    }
}

fn handle_close(stream: &mut impl Write, payload: &[u8], extended: &mut ExtendedSession) -> io::Result<ExtendedStep> {
    let Some((&kind, rest)) = payload.split_first()
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Close message");
    };
    let mut cursor = 0;
    let Some(name) = take_cstring(rest, &mut cursor)
    else {
        return extended_protocol_error(stream, "08P01", "malformed PostgreSQL Close target");
    };
    match kind {
        b'S' => {
            extended.statements.remove(name);
            // PostgreSQL invalidates any portal bound to the closed statement.
            extended.portals.retain(|_, portal| portal.statement_name != name);
        }
        b'P' => {
            extended.portals.remove(name);
        }
        _ => return extended_protocol_error(stream, "08P01", "unsupported PostgreSQL Close target"),
    }
    write_message(stream, b'3', &[])?;
    Ok(ExtendedStep::Continue)
}

/// Bind 只登记参数，真正执行推迟到 Describe / Execute，结果缓存避免重复写入。
fn prepare_portal(portal: &mut Portal, session: &mut TransactionSession, runtime: Option<&Arc<PgsqlSqlRuntime>>) {
    if portal.outcome.is_some() {
        return;
    }
    if session.is_aborted() {
        portal.outcome =
            Some(Err(("25P02".to_string(), "current transaction is aborted, commands ignored until end of transaction block".to_string())));
        return;
    }
    let Some(runtime) = runtime
    else {
        session.fail();
        portal.outcome = Some(Err(("0A000".to_string(), "PostgreSQL query execution is not implemented (no SQL runtime)".to_string())));
        return;
    };
    let sql = match substitute_sql_parameters(&portal.sql, &portal.parameters, SqlParameterStyle::DollarNumber) {
        Ok(sql) => sql,
        Err(error) => {
            session.fail();
            portal.outcome = Some(Err(statement_error("parameter", error)));
            return;
        }
    };
    portal.outcome = Some(run_statement(&sql, runtime, session));
}

/// Decodes the text form of `bytea`: `\x` hexadecimal or the escape format.
///
/// PostgreSQL accepts both spellings for a text-format parameter or literal, so
/// the gateway decodes them instead of refusing the parameter. A malformed
/// spelling reports `22P02` with the input it saw, which is what a client needs
/// to locate the offending bind.
fn parse_bytea_text(bytes: &[u8]) -> Result<Vec<u8>, (String, String)> {
    let invalid = || ("22P02".to_string(), format!("invalid input syntax for type bytea: \"{}\"", String::from_utf8_lossy(bytes)));
    if let Some(hex) = bytes.strip_prefix(b"\\x").or_else(|| bytes.strip_prefix(b"\\X")) {
        let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
        let digits: Vec<u8> = hex.bytes().filter(|byte| !byte.is_ascii_whitespace()).collect();
        if digits.len() % 2 != 0 {
            return Err(invalid());
        }
        let mut out = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks(2) {
            let high = (pair[0] as char).to_digit(16).ok_or_else(invalid)? as u8;
            let low = (pair[1] as char).to_digit(16).ok_or_else(invalid)? as u8;
            out.push(high << 4 | low);
        }
        return Ok(out);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'\\') {
            out.push(b'\\');
            index += 2;
            continue;
        }
        let octal = bytes.get(index + 1..index + 4).ok_or_else(invalid)?;
        if !octal.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            return Err(invalid());
        }
        out.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
        index += 4;
    }
    Ok(out)
}

/// Decodes one text-format parameter by its declared type OID.
///
/// The wire `Bind` path and the direct local session share this decode, so a
/// `$1` bound locally and the same value sent over the wire produce one
/// `SqlValue`.
pub fn parameter_value(oid: u32, bytes: &[u8]) -> Result<SqlValue, (String, String)> {
    // `bytea` 的文本形态是转义或十六进制拼写，不是原始 octets。网关按 PostgreSQL
    // 的 `byteain` 规则解码这两种拼写，而不是把转义字符本身当作载荷写进列里。
    if oid == OID_BYTEA {
        return parse_bytea_text(bytes).map(SqlValue::Bytes);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ("22021".to_string(), "parameter is not valid UTF-8".to_string()))?;
    match oid {
        0 => Ok(infer_parameter_value(text)),
        16 => match text.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "on" | "yes" | "y" => Ok(SqlValue::Boolean(true)),
            "f" | "false" | "0" | "off" | "no" | "n" => Ok(SqlValue::Boolean(false)),
            _ => Err(("22P02".to_string(), format!("invalid input syntax for type boolean: \"{text}\""))),
        },
        20 | 21 | 23 | 26 => text
            .parse::<i64>()
            .map(SqlValue::Integer)
            .map_err(|_| ("22P02".to_string(), format!("invalid input syntax for integer: \"{text}\""))),
        700 | 701 => text
            .parse::<f64>()
            .map(SqlValue::Number)
            .map_err(|_| ("22P02".to_string(), format!("invalid input syntax for double precision: \"{text}\""))),
        _ => Ok(SqlValue::String(text.to_string())),
    }
}

/// Decodes a binary-format Bind parameter by its declared OID. An unspecified
/// OID cannot be decoded without text syntax and is rejected explicitly.
fn binary_parameter_value(oid: u32, bytes: &[u8]) -> Result<SqlValue, (String, String)> {
    let width_error =
        |expected: usize| ("08P01".to_string(), format!("binary parameter for type OID {oid} expects {expected} bytes, got {}", bytes.len()));
    match oid {
        0 => Err(("42P18".to_string(), "cannot determine data type of a binary parameter".to_string())),
        OID_BOOL => match bytes {
            [0] => Ok(SqlValue::Boolean(false)),
            [1] => Ok(SqlValue::Boolean(true)),
            _ => Err(width_error(1)),
        },
        OID_INT2 => match bytes {
            [a, b] => Ok(SqlValue::Integer(i16::from_be_bytes([*a, *b]).into())),
            _ => Err(width_error(2)),
        },
        OID_INT4 => match bytes {
            [a, b, c, d] => Ok(SqlValue::Integer(i32::from_be_bytes([*a, *b, *c, *d]).into())),
            _ => Err(width_error(4)),
        },
        OID_INT8 => match bytes {
            [a, b, c, d, e, f, g, h] => Ok(SqlValue::Integer(i64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]))),
            _ => Err(width_error(8)),
        },
        OID_FLOAT4 => match bytes {
            [a, b, c, d] => Ok(SqlValue::Number(f32::from_be_bytes([*a, *b, *c, *d]).into())),
            _ => Err(width_error(4)),
        },
        OID_FLOAT8 => match bytes {
            [a, b, c, d, e, f, g, h] => Ok(SqlValue::Number(f64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]))),
            _ => Err(width_error(8)),
        },
        // `bytea` 的二进制格式就是原始 octets，原样收下，不做任何文本解码。
        OID_BYTEA => Ok(SqlValue::Bytes(bytes.to_vec())),
        // uuid 的二进制格式是 16 octets，转成连字符文本后与文本格式同一条路径。
        OID_UUID => match <[u8; 16]>::try_from(bytes) {
            Ok(octets) => Ok(SqlValue::String(uuid::Uuid::from_bytes(octets).hyphenated().to_string())),
            Err(_) => Err(width_error(16)),
        },
        OID_TIMESTAMP | OID_TIMESTAMPTZ => match bytes {
            [a, b, c, d, e, f, g, h] => {
                let micros = i64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]);
                Ok(SqlValue::String(timestamp_text(micros, oid == OID_TIMESTAMPTZ)))
            }
            _ => Err(width_error(8)),
        },
        OID_DATE => match bytes {
            [a, b, c, d] => Ok(SqlValue::String(date_text(i32::from_be_bytes([*a, *b, *c, *d])))),
            _ => Err(width_error(4)),
        },
        OID_JSON => utf8_parameter(bytes),
        // jsonb 的二进制格式是版本字节 1 加 JSON 文本。
        OID_JSONB => match bytes.split_first() {
            Some((1, rest)) => utf8_parameter(rest),
            _ => Err(("08P01".to_string(), "binary jsonb parameter must start with version byte 1".to_string())),
        },
        OID_NUMERIC => numeric_text_from_binary(bytes).map(SqlValue::String),
        // 一维数组参数解码成 PostgreSQL 数组文本，绑定期按量词比较展开成 IN 链。
        OID_TEXT_ARRAY | OID_VARCHAR_ARRAY | OID_UUID_ARRAY | OID_JSONB_ARRAY => pg_array_parameter(bytes),
        OID_TEXT | OID_VARCHAR | OID_NAME => std::str::from_utf8(bytes)
            .map(|text| SqlValue::String(text.to_string()))
            .map_err(|_| ("22021".to_string(), "binary parameter is not valid UTF-8".to_string())),
        _ => Err(("0A000".to_string(), format!("binary input for parameter type OID {oid} is not supported"))),
    }
}

/// Parse 未提供 OID 时的宽松推断，保持 `"007"` 这类文本不被数字转义。
fn infer_parameter_value(text: &str) -> SqlValue {
    if text.parse::<i64>().map(|value| value.to_string() == text).unwrap_or(false) {
        return SqlValue::Integer(text.parse().expect("checked integer"));
    }
    if (text.contains('.') || text.contains('e') || text.contains('E')) && text.parse::<f64>().is_ok() {
        return SqlValue::Number(text.parse().expect("checked float"));
    }
    SqlValue::String(text.to_string())
}

fn write_parameter_description(stream: &mut impl Write, oids: &[u32]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(2 + oids.len() * 4);
    payload.extend_from_slice(&(oids.len() as i16).to_be_bytes());
    for oid in oids {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    write_message(stream, b't', &payload)
}

fn extended_protocol_error(stream: &mut impl Write, code: &str, message: &str) -> io::Result<ExtendedStep> {
    write_error_response(stream, code, message)?;
    Ok(ExtendedStep::Failed)
}

fn take_bytes<'a>(payload: &'a [u8], cursor: &mut usize, length: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(length)?;
    let bytes = payload.get(*cursor..end)?;
    *cursor = end;
    Some(bytes)
}

fn take_cstring<'a>(payload: &'a [u8], cursor: &mut usize) -> Option<&'a str> {
    let rest = payload.get(*cursor..)?;
    let position = rest.iter().position(|byte| *byte == 0)?;
    let bytes = &rest[..position];
    *cursor += position + 1;
    std::str::from_utf8(bytes).ok()
}

fn take_i16(payload: &[u8], cursor: &mut usize) -> Option<i16> {
    let bytes = take_bytes(payload, cursor, 2)?;
    Some(i16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take_i32(payload: &[u8], cursor: &mut usize) -> Option<i32> {
    let bytes = take_bytes(payload, cursor, 4)?;
    Some(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_u32(payload: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes = take_bytes(payload, cursor, 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Case-insensitive lookup of one reported parameter.
///
/// The default table lives in `database.execution::pgsql` so `SHOW`,
/// `current_setting()` and the session GUC state all read one source.
/// `server_version` stays here because the server crate owns the banner.
fn pg_parameter(name: &str) -> Option<&'static str> {
    if name.trim().eq_ignore_ascii_case("server_version") {
        return Some(pg_server_version());
    }
    database_execution::pgsql::default_parameter(name)
}

/// PostgreSQL type OID for a described gateway field type.
///
/// The direct (local session) path reports the same OIDs the wire
/// `RowDescription` carries, so a host decodes both paths identically.
pub fn pg_oid(field_type: &SqlFieldType) -> u32 {
    match field_type.inner() {
        SqlFieldType::Int32 => OID_INT4,
        SqlFieldType::Int16 => OID_INT2,
        SqlFieldType::Int64 => OID_INT8,
        SqlFieldType::Float64 => OID_FLOAT8,
        SqlFieldType::Bool => OID_BOOL,
        SqlFieldType::Bytes => OID_BYTEA,
        SqlFieldType::Decimal => OID_NUMERIC,
        // 日期/时间按 PostgreSQL 自身的 OID 上报，值仍以 canonical 文本存储，
        // 二进制格式在 `pg_binary_value` 里换算，文本格式原样下发。
        SqlFieldType::Date => OID_DATE,
        SqlFieldType::Datetime => OID_TIMESTAMPTZ,
        // uuid 存储与比较是 16 字节二进制，wire 面按 `uuid` 上报，文本格式仍是
        // 连字符小写形式，二进制格式是 16 octets。
        SqlFieldType::Uuid => OID_UUID,
        // JSON/JSONB 以文本存储；上报 `jsonb` 让驱动把载荷解析成 JSON 值。
        SqlFieldType::Json => OID_JSONB,
        // `array_agg` 投影按元素类型上报 PostgreSQL 的数组 OID，驱动据此物化 `Vec<T>`。
        SqlFieldType::Array(element) => array_oid(&element.inner()),
        SqlFieldType::String | SqlFieldType::Unknown | SqlFieldType::Optional(_) => OID_TEXT,
    }
}

/// PostgreSQL array OID for an element type.
fn array_oid(element: &SqlFieldType) -> u32 {
    match element.inner() {
        SqlFieldType::Int16 => 1005,
        SqlFieldType::Int32 => 1007,
        SqlFieldType::Int64 => 1016,
        SqlFieldType::Float64 => 1022,
        SqlFieldType::Bool => 1000,
        SqlFieldType::Uuid => OID_UUID_ARRAY,
        SqlFieldType::Json => OID_JSONB_ARRAY,
        SqlFieldType::Decimal => 1231,
        SqlFieldType::Date => 1182,
        SqlFieldType::Datetime => 1185,
        SqlFieldType::Bytes => 1001,
        _ => OID_TEXT_ARRAY,
    }
}

/// Element OID carried inside the binary array payload.
fn array_element_oid(array_oid: u32) -> u32 {
    match array_oid {
        1005 => OID_INT2,
        1007 => OID_INT4,
        1016 => OID_INT8,
        1022 => OID_FLOAT8,
        1000 => OID_BOOL,
        OID_UUID_ARRAY => OID_UUID,
        OID_JSONB_ARRAY => OID_JSONB,
        1231 => OID_NUMERIC,
        1182 => OID_DATE,
        1185 => OID_TIMESTAMPTZ,
        1001 => OID_BYTEA,
        _ => OID_TEXT,
    }
}

/// Whether `oid` names one of the PostgreSQL array types the gateway emits.
fn is_array_oid(oid: u32) -> bool {
    matches!(oid, 1005 | 1007 | 1016 | 1022 | 1000 | OID_TEXT_ARRAY | OID_VARCHAR_ARRAY | OID_UUID_ARRAY | OID_JSONB_ARRAY | 1231 | 1182 | 1185 | 1001)
}

/// PostgreSQL RowDescription message ('T') — catalog-derived OIDs and the
/// format codes requested at Bind.
fn write_row_description(stream: &mut impl Write, columns: &[SqlColumn], formats: &[i16]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(64);
    payload.push(b'T');
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for (index, column) in columns.iter().enumerate() {
        let oid = pg_oid(&column.field_type);
        body.extend_from_slice(column.name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0_i32.to_be_bytes()); // table_oid = 0
        body.extend_from_slice(&0_i16.to_be_bytes()); // column_attr = 0
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&type_size(oid).to_be_bytes());
        body.extend_from_slice(&(-1_i32).to_be_bytes()); // type_mod = -1
        body.extend_from_slice(&formats.get(index).copied().unwrap_or(0).to_be_bytes());
    }
    payload.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
    payload.extend_from_slice(&body);
    stream.write_all(&payload)
}

fn type_size(oid: u32) -> i16 {
    match oid {
        OID_BOOL => 1,
        OID_INT2 => 2,
        OID_INT4 | OID_FLOAT4 | OID_DATE => 4,
        OID_INT8 | OID_FLOAT8 | OID_TIMESTAMP | OID_TIMESTAMPTZ => 8,
        OID_UUID => 16,
        _ => -1,
    }
}

/// PostgreSQL DataRow message ('D') — text or binary encoding per column.
fn write_data_row(stream: &mut impl Write, row: &[SqlValue], columns: &[SqlColumn], formats: &[i16]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(64);
    payload.push(b'D');
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(&(row.len() as i16).to_be_bytes());
    for (index, value) in row.iter().enumerate() {
        if matches!(value, SqlValue::Null) {
            body.extend_from_slice(&(-1_i32).to_be_bytes());
            continue;
        }
        let oid = columns.get(index).map(|column| pg_oid(&column.field_type)).unwrap_or(OID_TEXT);
        let bytes = if formats.get(index).copied().unwrap_or(0) == 1 {
            match pg_binary_value(oid, value) {
                Ok(bytes) => bytes,
                Err((code, message)) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{code}: {message}")));
                }
            }
        }
        else {
            sql_value_to_pg_text(value).into_bytes()
        };
        body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
        body.extend_from_slice(&bytes);
    }
    payload.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
    payload.extend_from_slice(&body);
    stream.write_all(&payload)
}

/// Encodes a value in the big-endian binary representation of its column OID.
fn pg_binary_value(oid: u32, value: &SqlValue) -> Result<Vec<u8>, (String, String)> {
    match oid {
        OID_BOOL => Ok(vec![u8::from(truthy_value(value)?)]),
        OID_INT2 => {
            let value = integer_value(value)?;
            let narrow = i16::try_from(value).map_err(|_| ("22003".to_string(), format!("smallint out of range: {value}")))?;
            Ok(narrow.to_be_bytes().to_vec())
        }
        OID_INT4 => {
            let value = integer_value(value)?;
            let narrow = i32::try_from(value).map_err(|_| ("22003".to_string(), format!("integer out of range: {value}")))?;
            Ok(narrow.to_be_bytes().to_vec())
        }
        OID_INT8 => Ok(integer_value(value)?.to_be_bytes().to_vec()),
        OID_FLOAT4 => Ok((float_value(value)? as f32).to_be_bytes().to_vec()),
        OID_FLOAT8 => Ok(float_value(value)?.to_be_bytes().to_vec()),
        OID_TEXT | OID_VARCHAR | OID_NAME => Ok(sql_value_to_pg_text(value).into_bytes()),
        OID_BYTEA => match value {
            SqlValue::Bytes(bytes) => Ok(bytes.clone()),
            other => Err(("22P02".to_string(), format!("value {other:?} is not a bytea payload"))),
        },
        OID_UUID => {
            let text = text_value(value)?;
            match uuid::Uuid::parse_str(&text) {
                Ok(parsed) => Ok(parsed.as_bytes().to_vec()),
                Err(_) => Err(("22P02".to_string(), format!("invalid input syntax for type uuid: \"{text}\""))),
            }
        }
        OID_TIMESTAMP | OID_TIMESTAMPTZ => {
            let text = text_value(value)?;
            match timestamp_micros(&text) {
                Some(micros) => Ok(micros.to_be_bytes().to_vec()),
                None => Err(("22007".to_string(), format!("invalid input syntax for timestamp: \"{text}\""))),
            }
        }
        OID_DATE => {
            let text = text_value(value)?;
            match date_days(&text) {
                Some(days) => Ok(days.to_be_bytes().to_vec()),
                None => Err(("22007".to_string(), format!("invalid input syntax for date: \"{text}\""))),
            }
        }
        OID_JSON => Ok(text_value(value)?.into_bytes()),
        OID_JSONB => {
            let text = text_value(value)?;
            let mut bytes = Vec::with_capacity(text.len() + 1);
            bytes.push(1);
            bytes.extend_from_slice(text.as_bytes());
            Ok(bytes)
        }
        OID_NUMERIC => numeric_binary_from_text(&text_value(value)?),
        oid if is_array_oid(oid) => pg_binary_array(oid, value),
        _ => Err(("0A000".to_string(), format!("binary output for type OID {oid} is not supported"))),
    }
}

/// PostgreSQL binary array payload: header plus one length-prefixed element each.
fn pg_binary_array(oid: u32, value: &SqlValue) -> Result<Vec<u8>, (String, String)> {
    let SqlValue::Array(items) = value
    else {
        return Err(("22P02".to_string(), format!("value {value:?} is not an array payload")));
    };
    let element_oid = array_element_oid(oid);
    let mut out = Vec::with_capacity(20 + items.len() * 8);
    out.extend_from_slice(&1_i32.to_be_bytes()); // ndim
    out.extend_from_slice(&i32::from(items.iter().any(|item| matches!(item, SqlValue::Null))).to_be_bytes()); // hasnull
    out.extend_from_slice(&element_oid.to_be_bytes());
    out.extend_from_slice(&(items.len() as i32).to_be_bytes()); // dim length
    out.extend_from_slice(&1_i32.to_be_bytes()); // lower bound
    for item in items {
        if matches!(item, SqlValue::Null) {
            out.extend_from_slice(&(-1_i32).to_be_bytes());
            continue;
        }
        let bytes = pg_binary_value(element_oid, item)?;
        out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// PostgreSQL array text literal: `{a,b}` with quoting for special elements.
fn pg_array_text(items: &[SqlValue]) -> String {
    let mut out = String::from("{");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&pg_array_element_text(item));
    }
    out.push('}');
    out
}

fn pg_array_element_text(value: &SqlValue) -> String {
    if matches!(value, SqlValue::Null) {
        return "NULL".to_owned();
    }
    let text = sql_value_to_pg_text(value);
    let needs_quotes = text.is_empty()
        || text.eq_ignore_ascii_case("null")
        || text.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
    if needs_quotes {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
    else {
        text
    }
}

fn integer_value(value: &SqlValue) -> Result<i64, (String, String)> {
    match value {
        SqlValue::Integer(value) => Ok(*value),
        SqlValue::Boolean(value) => Ok(i64::from(*value)),
        SqlValue::Number(value) if value.fract() == 0.0 => Ok(*value as i64),
        SqlValue::String(text) => {
            text.parse::<i64>().map_err(|_| ("22P02".to_string(), format!("invalid input syntax for integer: \"{text}\"")))
        }
        other => Err(("22P02".to_string(), format!("value {other:?} is not a binary integer"))),
    }
}

fn float_value(value: &SqlValue) -> Result<f64, (String, String)> {
    match value {
        SqlValue::Integer(value) => Ok(*value as f64),
        SqlValue::Number(value) => Ok(*value),
        SqlValue::Boolean(value) => Ok(if *value { 1.0 } else { 0.0 }),
        SqlValue::String(text) => {
            text.parse::<f64>().map_err(|_| ("22P02".to_string(), format!("invalid input syntax for double precision: \"{text}\"")))
        }
        other => Err(("22P02".to_string(), format!("value {other:?} is not a binary float"))),
    }
}

fn truthy_value(value: &SqlValue) -> Result<bool, (String, String)> {
    match value {
        SqlValue::Boolean(value) => Ok(*value),
        SqlValue::Integer(value) => Ok(*value != 0),
        SqlValue::String(text) => match text.as_str() {
            "t" | "true" | "1" => Ok(true),
            "f" | "false" | "0" => Ok(false),
            _ => Err(("22P02".to_string(), format!("invalid input syntax for type boolean: \"{text}\""))),
        },
        other => Err(("22P02".to_string(), format!("value {other:?} is not a binary boolean"))),
    }
}

/// Text payload of one stored value in the PostgreSQL text format the gateway
/// writes to `DataRow` cells.
pub fn sql_value_to_pg_text(value: &SqlValue) -> String {
    match value {
        SqlValue::Integer(n) => n.to_string(),
        SqlValue::Number(n) => format!("{n}"),
        SqlValue::String(s) => s.clone(),
        // PostgreSQL 的 `bytea` 文本格式就是十六进制拼写，可以无损表达任意 octets。
        SqlValue::Bytes(bytes) => format!("\\x{}", encode_hex(bytes)),
        SqlValue::Boolean(b) => {
            if *b {
                "t".into()
            }
            else {
                "f".into()
            }
        }
        SqlValue::Array(items) => pg_array_text(items),
        SqlValue::Null => unreachable!(),
    }
}

/// Days between 1970-01-01 and PostgreSQL's 2000-01-01 epoch.
const PG_EPOCH_DAYS: i64 = 10_957;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn utf8_parameter(bytes: &[u8]) -> Result<SqlValue, (String, String)> {
    std::str::from_utf8(bytes)
        .map(|text| SqlValue::String(text.to_string()))
        .map_err(|_| ("22021".to_string(), "binary parameter is not valid UTF-8".to_string()))
}

/// Text payload of a scalar value the wire formats accept as text.
fn text_value(value: &SqlValue) -> Result<String, (String, String)> {
    match value {
        SqlValue::String(text) => Ok(text.clone()),
        SqlValue::Integer(number) => Ok(number.to_string()),
        SqlValue::Number(number) => Ok(format!("{number}")),
        other => Err(("22P02".to_string(), format!("value {other:?} has no text form"))),
    }
}

/// Days from 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Parses `YYYY-MM-DD` into days since PostgreSQL's 2000-01-01 epoch.
fn date_days(text: &str) -> Option<i32> {
    let text = text.trim();
    let mut parts = text.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day) - PG_EPOCH_DAYS;
    i32::try_from(days).ok()
}

/// Renders days since 2000-01-01 as `YYYY-MM-DD`.
fn date_text(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days) + PG_EPOCH_DAYS);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Parses `YYYY-MM-DD[ T]HH:MM:SS[.ffffff][+HH[:MM]|Z]` into microseconds since
/// 2000-01-01 UTC plus the source offset in minutes.
fn parse_timestamp(text: &str) -> Option<(i64, i32)> {
    let text = text.trim();
    let split = text.find(['T', ' '])?;
    let (date, rest) = text.split_at(split);
    let rest = rest[1..].trim();

    let mut body = rest;
    let mut offset = 0_i32;
    if let Some(stripped) = body.strip_suffix('Z').or_else(|| body.strip_suffix('z')) {
        body = stripped;
    }
    else if let Some(position) = body.rfind(['+', '-']) {
        // 时区偏移从第 3 个字符之后才可能出现，负号不会与日期相连。
        if position >= 3 {
            let (clock, zone) = body.split_at(position);
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let zone = &zone[1..];
            let (hours, minutes) = match zone.split_once(':') {
                Some((hours, minutes)) => (hours, minutes),
                None if zone.len() >= 3 => (&zone[..2], &zone[2..]),
                None => (zone, "0"),
            };
            let hours: i32 = hours.parse().ok()?;
            let minutes: i32 = minutes.parse().ok()?;
            offset = sign * (hours * 60 + minutes);
            body = clock;
        }
    }

    let mut clock = body.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let seconds = clock.next().unwrap_or("0");
    let (second, fraction) = match seconds.split_once('.') {
        Some((second, fraction)) => (second, fraction),
        None => (seconds, ""),
    };
    let second: i64 = second.parse().ok()?;
    let mut micros = 0_i64;
    if !fraction.is_empty() {
        let digits = fraction.trim_end_matches(|c: char| !c.is_ascii_digit());
        if digits.len() > 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        micros = digits.parse::<i64>().ok()? * 10_i64.pow(6 - digits.len() as u32);
    }

    let days = days_from_civil(
        date.split('-').next()?.parse().ok()?,
        date.split('-').nth(1)?.parse().ok()?,
        date.split('-').nth(2)?.parse().ok()?,
    ) - PG_EPOCH_DAYS;
    let total = days * MICROS_PER_DAY + (hour * 3_600 + minute * 60 + second) * 1_000_000 + micros - i64::from(offset) * 60_000_000;
    Some((total, offset))
}

/// Microseconds since PostgreSQL's epoch for a timestamp spelling.
fn timestamp_micros(text: &str) -> Option<i64> {
    parse_timestamp(text).map(|(micros, _)| micros)
}

/// Renders microseconds since 2000-01-01 UTC as PostgreSQL's text timestamp.
fn timestamp_text(micros: i64, with_zone: bool) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY) + PG_EPOCH_DAYS;
    let rest = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let seconds = rest / 1_000_000;
    let fraction = rest % 1_000_000;
    let mut text = format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        text.push('.');
        text.push_str(digits.trim_end_matches('0'));
    }
    if with_zone {
        text.push_str("+00");
    }
    text
}

/// Encodes decimal text into PostgreSQL's binary `numeric` payload.
fn numeric_binary_from_text(text: &str) -> Result<Vec<u8>, (String, String)> {
    let invalid = || ("22P02".to_string(), format!("invalid input syntax for type numeric: \"{text}\""));
    let text = text.trim();
    if text.eq_ignore_ascii_case("nan") {
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&0_i16.to_be_bytes());
        bytes.extend_from_slice(&0_i16.to_be_bytes());
        bytes.extend_from_slice(&0xC000_u16.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        return Ok(bytes);
    }
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let body = body.split(['e', 'E']).next().unwrap_or(body);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(invalid());
    }
    let (integer, fraction) = match body.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (body, ""),
    };
    if integer.is_empty() && fraction.is_empty() {
        return Err(invalid());
    }
    let scale = fraction.len().min(u16::MAX as usize);

    let mut integer_groups: Vec<u16> = Vec::new();
    let padded_integer = {
        let pad = (4 - integer.len() % 4) % 4;
        format!("{}{integer}", "0".repeat(pad))
    };
    for chunk in padded_integer.as_bytes().chunks(4) {
        if chunk.is_empty() {
            continue;
        }
        integer_groups.push(std::str::from_utf8(chunk).map_err(|_| invalid())?.parse::<u16>().map_err(|_| invalid())?);
    }
    let mut fraction_groups: Vec<u16> = Vec::new();
    let padded_fraction = {
        let pad = (4 - fraction.len() % 4) % 4;
        format!("{fraction}{}", "0".repeat(pad))
    };
    for chunk in padded_fraction.as_bytes().chunks(4) {
        if chunk.is_empty() {
            continue;
        }
        fraction_groups.push(std::str::from_utf8(chunk).map_err(|_| invalid())?.parse::<u16>().map_err(|_| invalid())?);
    }

    let mut weight = integer_groups.len() as i32 - 1;
    let mut digits = integer_groups;
    digits.extend(fraction_groups);
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        weight = 0;
    }
    let sign = if negative && !digits.is_empty() { 0x4000_u16 } else { 0 };

    let mut bytes = Vec::with_capacity(8 + digits.len() * 2);
    bytes.extend_from_slice(&(digits.len() as i16).to_be_bytes());
    bytes.extend_from_slice(&(weight as i16).to_be_bytes());
    bytes.extend_from_slice(&sign.to_be_bytes());
    bytes.extend_from_slice(&(scale as u16).to_be_bytes());
    for digit in digits {
        bytes.extend_from_slice(&digit.to_be_bytes());
    }
    Ok(bytes)
}

/// Decodes PostgreSQL's binary `numeric` payload back into decimal text.
fn numeric_text_from_binary(bytes: &[u8]) -> Result<String, (String, String)> {
    let malformed = || ("08P01".to_string(), format!("malformed binary numeric payload ({} bytes)", bytes.len()));
    if bytes.len() < 8 {
        return Err(malformed());
    }
    let read_u16 = |offset: usize| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
    let ndigits = i16::from_be_bytes([bytes[0], bytes[1]]);
    let weight = i16::from_be_bytes([bytes[2], bytes[3]]);
    let sign = read_u16(4);
    let scale = read_u16(6) as usize;
    if ndigits < 0 || bytes.len() != 8 + ndigits as usize * 2 {
        return Err(malformed());
    }
    if sign == 0xC000 {
        return Ok("NaN".to_owned());
    }
    let digits: Vec<u16> = (0..ndigits as usize).map(|index| read_u16(8 + index * 2)).collect();

    let integer_groups = (weight + 1).max(0) as usize;
    let fraction_groups = ((scale + 3) / 4).max(0);
    let mut integer = String::new();
    for position in 0..integer_groups {
        let digit = digits.get(position).copied().unwrap_or(0);
        if position == 0 {
            integer.push_str(&digit.to_string());
        }
        else {
            integer.push_str(&format!("{digit:04}"));
        }
    }
    if integer.is_empty() {
        integer.push('0');
    }
    let mut fraction = String::new();
    for index in 0..fraction_groups {
        let digit = digits.get(integer_groups + index).copied().unwrap_or(0);
        fraction.push_str(&format!("{digit:04}"));
    }
    fraction.truncate(scale);
    let sign_text = if sign == 0x4000 { "-" } else { "" };
    if scale == 0 {
        Ok(format!("{sign_text}{integer}"))
    }
    else {
        Ok(format!("{sign_text}{integer}.{fraction}"))
    }
}

/// Decodes a one-dimensional binary array parameter into PostgreSQL array text.
///
/// The binder parses `{a,b}` back into element literals for `ANY`/`ALL`, so the
/// wire can carry `uuid[]`, `text[]` and `jsonb[]` without teaching the storage
/// layer a first-class array type.
fn pg_array_parameter(bytes: &[u8]) -> Result<SqlValue, (String, String)> {
    let malformed = || ("08P01".to_string(), "malformed binary array parameter".to_string());
    if bytes.len() < 20 {
        return Err(malformed());
    }
    let read_i32 = |offset: usize| i32::from_be_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]);
    let dimensions = read_i32(0);
    // 头是 ndim(4) | has-nulls 标志(4) | 元素 OID(4) | 维度长度(4) | 下界(4)。
    let element_oid = read_i32(8) as u32;
    if dimensions != 1 {
        return Err(("0A000".to_string(), format!("only one-dimensional array parameters are supported, got {dimensions}")));
    }
    let length = read_i32(12);
    if length < 0 {
        return Err(malformed());
    }
    let mut position = 20;
    let mut elements = Vec::with_capacity(length as usize);
    for _ in 0..length {
        if bytes.len() < position + 4 {
            return Err(malformed());
        }
        let size = read_i32(position);
        position += 4;
        if size < 0 {
            elements.push(None);
            continue;
        }
        let size = size as usize;
        if bytes.len() < position + size {
            return Err(malformed());
        }
        let element = &bytes[position..position + size];
        position += size;
        elements.push(Some(array_element_text(element_oid, element)?));
    }
    Ok(SqlValue::String(render_pg_array(&elements)))
}

fn array_element_text(oid: u32, bytes: &[u8]) -> Result<String, (String, String)> {
    match oid {
        OID_UUID => match <[u8; 16]>::try_from(bytes) {
            Ok(octets) => Ok(uuid::Uuid::from_bytes(octets).hyphenated().to_string()),
            Err(_) => Err(("08P01".to_string(), "uuid array element must be 16 octets".to_string())),
        },
        OID_TEXT | OID_VARCHAR | OID_NAME | OID_JSON => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ("22021".to_string(), "array element is not valid UTF-8".to_string())),
        OID_JSONB => match bytes.split_first() {
            Some((1, rest)) => std::str::from_utf8(rest)
                .map(str::to_owned)
                .map_err(|_| ("22021".to_string(), "jsonb array element is not valid UTF-8".to_string())),
            _ => Err(("08P01".to_string(), "binary jsonb array element must start with version byte 1".to_string())),
        },
        other => Err(("0A000".to_string(), format!("array element type OID {other} is not supported"))),
    }
}

fn render_pg_array(elements: &[Option<String>]) -> String {
    let mut rendered = String::from("{");
    for (index, element) in elements.iter().enumerate() {
        if index != 0 {
            rendered.push(',');
        }
        match element {
            None => rendered.push_str("NULL"),
            Some(text) => {
                let needs_quotes = text.is_empty()
                    || text.eq_ignore_ascii_case("null")
                    || text.chars().any(|character| matches!(character, ',' | '{' | '}' | '"' | '\\') || character.is_whitespace());
                if needs_quotes {
                    rendered.push('"');
                    rendered.push_str(&text.replace('\\', "\\\\").replace('"', "\\\""));
                    rendered.push('"');
                }
                else {
                    rendered.push_str(text);
                }
            }
        }
    }
    rendered.push('}');
    rendered
}

struct OwnedStartup {
    code: u32,
    payload: Vec<u8>,
}

struct OwnedMessage {
    tag: u8,
    payload: Vec<u8>,
}

fn read_startup(stream: &mut impl Read, pending: &mut Vec<u8>) -> io::Result<OwnedStartup> {
    loop {
        match decode_startup(pending, MAX_STARTUP_FRAME) {
            Ok(Some(packet)) => {
                let result = OwnedStartup { code: packet.code, payload: packet.payload.to_vec() };
                pending.drain(..packet.consumed);
                return Ok(result);
            }
            Ok(None) => read_more(stream, pending)?,
            Err(error) => return Err(frame_error(error)),
        }
    }
}

fn read_message(stream: &mut impl Read, pending: &mut Vec<u8>) -> io::Result<Option<OwnedMessage>> {
    loop {
        match decode_message(pending, MAX_MESSAGE_FRAME) {
            Ok(Some(message)) => {
                let result = OwnedMessage { tag: message.tag, payload: message.payload.to_vec() };
                pending.drain(..message.consumed);
                return Ok(Some(result));
            }
            Ok(None) => {
                if let Err(error) = read_more(stream, pending) {
                    if error.kind() == io::ErrorKind::UnexpectedEof {
                        return if pending.is_empty() {
                            Ok(None)
                        }
                        else {
                            Err(io::Error::new(io::ErrorKind::InvalidData, "truncated PostgreSQL message"))
                        };
                    }
                    return Err(error);
                }
            }
            Err(error) => return Err(frame_error(error)),
        }
    }
}

fn read_more(stream: &mut impl Read, pending: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0; 8192];
    let read = stream.read(&mut chunk)?;
    if read == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "PostgreSQL peer closed"));
    }
    pending.extend_from_slice(&chunk[..read]);
    Ok(())
}

fn send_authentication_ok(stream: &mut impl Write) -> io::Result<()> {
    write_message(stream, b'R', &0_u32.to_be_bytes())
}

/// Runs the configured authentication exchange.
///
/// `Ok(None)` means the client may proceed. `Ok(Some(failure))` carries the
/// SQLSTATE and message the session reports as a fatal error, which is how
/// PostgreSQL answers a rejected password (`28P01`) or an unsupported method
/// (`28000`).
fn authenticate(
    stream: &mut (impl Read + Write),
    pending: &mut Vec<u8>,
    parameters: &StartupParameters,
    auth: Option<&PgAuthentication>,
    objects: Option<&database_storage::pgsql::PgObjectCatalog>,
) -> io::Result<Option<AuthFailure>> {
    let Some(auth) = auth
    else {
        return Ok(None);
    };
    let user = parameters.user();
    if !auth.matches_role(user) {
        return Ok(Some(AuthFailure::password(user)));
    }
    match auth {
        PgAuthentication::Trust => Ok(None),
        PgAuthentication::Password { password, .. } => {
            write_auth_request(stream, 3, &[])?;
            let Some(message) = read_message(stream, pending)?
            else {
                return Ok(Some(AuthFailure::protocol("client disconnected during password authentication")));
            };
            let submitted = cstring(&message.payload);
            let accepted = submitted.is_some_and(|submitted| constant_time_eq(submitted.as_bytes(), password.as_bytes()));
            Ok((!accepted).then(|| AuthFailure::password(user)))
        }
        PgAuthentication::Md5 { .. } => {
            let mut salt = [0u8; 4];
            fill_random(&mut salt)?;
            write_auth_request(stream, 5, &salt)?;
            let Some(message) = read_message(stream, pending)?
            else {
                return Ok(Some(AuthFailure::protocol("client disconnected during MD5 authentication")));
            };
            let submitted = cstring(&message.payload);
            let expected = auth.expected_md5(user, &salt).unwrap_or_default();
            let accepted = submitted.is_some_and(|submitted| constant_time_eq(submitted.as_bytes(), expected.as_bytes()));
            Ok((!accepted).then(|| AuthFailure::password(user)))
        }
        PgAuthentication::ScramSha256 { password, .. } => {
            let mut suffix = [0u8; 18];
            fill_random(&mut suffix)?;
            let salt = scram_salt()?;
            let mut server = ScramServer::new(password.clone(), salt, 4096, base64_encode(&suffix));
            run_scram_exchange(stream, pending, &mut server, user)
        }
        PgAuthentication::RolePassword(method) => {
            // The credential lives in the cluster catalog: a role without a
            // stored verifier, or a role the catalog does not know, fails exactly
            // like a wrong password.
            let Some(objects) = objects
            else {
                return Ok(Some(AuthFailure::unsupported("role password authentication requires the cluster object catalog")));
            };
            let Some(stored) = objects.role(user).and_then(|role| role.password)
            else {
                return Ok(Some(AuthFailure::password(user)));
            };
            match method {
                super::auth::PgPasswordMethod::Cleartext => {
                    write_auth_request(stream, 3, &[])?;
                    let Some(message) = read_message(stream, pending)?
                    else {
                        return Ok(Some(AuthFailure::protocol("client disconnected during password authentication")));
                    };
                    let submitted = cstring(&message.payload);
                    let accepted = submitted.is_some_and(|submitted| {
                        let derived = database_protocol_pgsql::auth::md5_hex(format!("{submitted}{user}").as_bytes());
                        constant_time_eq(derived.as_bytes(), stored.md5.as_bytes())
                    });
                    Ok((!accepted).then(|| AuthFailure::password(user)))
                }
                super::auth::PgPasswordMethod::Md5 => {
                    let mut salt = [0u8; 4];
                    fill_random(&mut salt)?;
                    write_auth_request(stream, 5, &salt)?;
                    let Some(message) = read_message(stream, pending)?
                    else {
                        return Ok(Some(AuthFailure::protocol("client disconnected during MD5 authentication")));
                    };
                    let submitted = cstring(&message.payload);
                    let expected = stored.md5_response(&salt);
                    let accepted = submitted.is_some_and(|submitted| constant_time_eq(submitted.as_bytes(), expected.as_bytes()));
                    Ok((!accepted).then(|| AuthFailure::password(user)))
                }
                super::auth::PgPasswordMethod::ScramSha256 => {
                    let Some((verifier, salt)) = stored.scram()
                    else {
                        return Ok(Some(AuthFailure::unsupported(format!("the stored verifier of role \"{user}\" is malformed"))));
                    };
                    let mut suffix = [0u8; 18];
                    fill_random(&mut suffix)?;
                    let mut server = ScramServer::from_verifier(verifier, salt, stored.scram.iterations, base64_encode(&suffix));
                    run_scram_exchange(stream, pending, &mut server, user)
                }
            }
        }
    }
}

/// Server salt for a SCRAM exchange derived from the configured password.
fn scram_salt() -> io::Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    fill_random(&mut salt)?;
    Ok(salt.to_vec())
}

/// Runs one SCRAM exchange: mechanism selection, challenge, and verification.
fn run_scram_exchange(
    stream: &mut (impl Read + Write),
    pending: &mut Vec<u8>,
    server: &mut ScramServer,
    user: &str,
) -> io::Result<Option<AuthFailure>> {
    // `AuthenticationSASL` lists the offered mechanisms.
    let mut mechanisms = Vec::new();
    mechanisms.extend_from_slice(b"SCRAM-SHA-256\0\0");
    write_auth_request(stream, 10, &mechanisms)?;
    let Some(initial) = read_message(stream, pending)?
    else {
        return Ok(Some(AuthFailure::protocol("client disconnected before the SASL initial response")));
    };
    let mut cursor = 0;
    let Some(mechanism) = take_cstring(&initial.payload, &mut cursor)
    else {
        return Ok(Some(AuthFailure::protocol("malformed SASL initial response")));
    };
    if mechanism != "SCRAM-SHA-256" {
        return Ok(Some(AuthFailure::unsupported(format!("the gateway only offers SCRAM-SHA-256, the client selected `{mechanism}`"))));
    }
    let Some(length) = take_i32(&initial.payload, &mut cursor)
    else {
        return Ok(Some(AuthFailure::protocol("malformed SASL initial response")));
    };
    if length < 0 {
        return Ok(Some(AuthFailure::protocol("SASL initial response carries no client-first message")));
    }
    let Some(client_first) = take_bytes(&initial.payload, &mut cursor, length as usize)
    else {
        return Ok(Some(AuthFailure::protocol("truncated SASL initial response")));
    };
    let Ok(client_first) = std::str::from_utf8(client_first)
    else {
        return Ok(Some(AuthFailure::protocol("SASL client-first is not valid UTF-8")));
    };
    let Ok(server_first) = server.begin(client_first)
    else {
        return Ok(Some(AuthFailure::protocol("malformed SCRAM client-first message")));
    };
    write_auth_request(stream, 11, server_first.as_bytes())?;
    let Some(response) = read_message(stream, pending)?
    else {
        return Ok(Some(AuthFailure::protocol("client disconnected before the SASL response")));
    };
    let Ok(client_final) = std::str::from_utf8(&response.payload)
    else {
        return Ok(Some(AuthFailure::protocol("SASL client-final is not valid UTF-8")));
    };
    match server.finish(client_final) {
        Ok(server_final) => {
            write_auth_request(stream, 12, server_final.as_bytes())?;
            Ok(None)
        }
        Err(_) => Ok(Some(AuthFailure::password(user))),
    }
}

fn write_auth_request(stream: &mut impl Write, code: u32, extra: &[u8]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(4 + extra.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(extra);
    write_message(stream, b'R', &payload)
}

/// Reads a NUL-terminated string from a message payload.
fn cstring(payload: &[u8]) -> Option<String> {
    let end = payload.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&payload[..end]).ok().map(str::to_owned)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    boost_random::Secure::system()
        .fill(bytes)
        .map_err(|error| io::Error::other(format!("cannot generate an authentication challenge: {error}")))
}

fn base64_encode(bytes: &[u8]) -> String {
    use data_encoding::representation::Base64;
    Base64::standard_strict().encode_text(bytes)
}

fn send_parameter(stream: &mut impl Write, key: &str, value: &str) -> io::Result<()> {
    let mut payload = Vec::with_capacity(key.len() + value.len() + 2);
    payload.extend_from_slice(key.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    write_message(stream, b'S', &payload)
}

fn send_backend_key(stream: &mut impl Write) -> io::Result<(i32, i32)> {
    let process_id = std::process::id() as i32;
    let secret_key = NEXT_SECRET.fetch_add(1, Ordering::Relaxed) as i32;
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&process_id.to_be_bytes());
    payload.extend_from_slice(&secret_key.to_be_bytes());
    write_message(stream, b'K', &payload)?;
    Ok((process_id, secret_key))
}

/// Drops one session's cancel flag when its connection ends.
struct CancelGuard {
    runtime: Option<Arc<PgsqlSqlRuntime>>,
    process_id: i32,
    secret_key: i32,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.cancels.unregister(self.process_id, self.secret_key);
        }
    }
}

/// Writes every queued `NotificationResponse` ('A'), then `ReadyForQuery`.
///
/// PostgreSQL delivers notifications before the next ready message, so a
/// client that ran `LISTEN` in an earlier statement observes a `NOTIFY` from
/// another session at the end of its next command.
fn send_ready(stream: &mut impl Write, session: &mut TransactionSession) -> io::Result<()> {
    for notification in session.take_notifications() {
        let mut payload = Vec::with_capacity(8 + notification.channel.len() + notification.payload.len());
        payload.extend_from_slice(&notification.process_id.to_be_bytes());
        payload.extend_from_slice(notification.channel.as_bytes());
        payload.push(0);
        payload.extend_from_slice(notification.payload.as_bytes());
        payload.push(0);
        write_message(stream, b'A', &payload)?;
    }
    let ready = session.ready_byte();
    write_message(stream, b'Z', &[ready])
}

fn write_command_complete(stream: &mut impl Write, tag: &str) -> io::Result<()> {
    let mut payload = Vec::with_capacity(tag.len() + 1);
    payload.extend_from_slice(tag.as_bytes());
    payload.push(0);
    write_message(stream, b'C', &payload)
}

fn write_error_response(stream: &mut impl Write, code: &str, message: &str) -> io::Result<()> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(b"ERROR\0C");
    payload.extend_from_slice(code.as_bytes());
    payload.extend_from_slice(b"\0M");
    payload.extend_from_slice(message.as_bytes());
    payload.extend_from_slice(b"\0\0");
    write_message(stream, b'E', &payload)
}

/// Annotates the first ErrorResponse in a buffered statement response with its
/// batch position and string offset.
///
/// Returns `None` when the statement produced no ErrorResponse, in which case
/// the buffer can be forwarded unchanged.
fn annotate_batch_error(buffer: &[u8], index: usize, total: usize, offset: usize) -> Option<Vec<u8>> {
    let mut cursor = 0usize;
    while cursor + 5 <= buffer.len() {
        let length = u32::from_be_bytes([buffer[cursor + 1], buffer[cursor + 2], buffer[cursor + 3], buffer[cursor + 4]]) as usize;
        if length < 4 || cursor + 1 + length > buffer.len() {
            return None;
        }
        let payload_start = cursor + 5;
        let payload_end = cursor + 1 + length;
        if buffer[cursor] == b'E' {
            let prefix = format!("statement {index} of {total} failed at batch offset {offset}: ");
            let payload = annotate_error_payload(&buffer[payload_start..payload_end], &prefix);
            let mut output = Vec::with_capacity(buffer.len() + prefix.len());
            output.extend_from_slice(&buffer[..cursor]);
            output.push(b'E');
            output.extend_from_slice(&((payload.len() as u32) + 4).to_be_bytes());
            output.extend_from_slice(&payload);
            output.extend_from_slice(&buffer[payload_end..]);
            return Some(output);
        }
        cursor = payload_end;
    }
    None
}

/// Prepends `prefix` to the `M` (message) field of an ErrorResponse payload.
fn annotate_error_payload(payload: &[u8], prefix: &str) -> Vec<u8> {
    let mut fields: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut cursor = 0usize;
    while cursor < payload.len() && payload[cursor] != 0 {
        let code = payload[cursor];
        let start = cursor + 1;
        let mut end = start;
        while end < payload.len() && payload[end] != 0 {
            end += 1;
        }
        if end >= payload.len() {
            break;
        }
        fields.push((code, payload[start..end].to_vec()));
        cursor = end + 1;
    }
    let mut output = Vec::with_capacity(payload.len() + prefix.len());
    for (code, value) in fields {
        output.push(code);
        if code == b'M' {
            output.extend_from_slice(prefix.as_bytes());
        }
        output.extend_from_slice(&value);
        output.push(0);
    }
    output.push(0);
    output
}

fn write_fatal(stream: &mut impl Write, code: &str, message: &str) -> io::Result<()> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(b"FATAL\0C");
    payload.extend_from_slice(code.as_bytes());
    payload.extend_from_slice(b"\0M");
    payload.extend_from_slice(message.as_bytes());
    payload.extend_from_slice(b"\0\0");
    write_message(stream, b'E', &payload)
}

fn write_message(stream: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let encoded = encode_message(tag, payload, MAX_MESSAGE_FRAME).map_err(frame_error)?;
    stream.write_all(&encoded)
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
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "PostgreSQL service stopping"));
            }
            match operation(&mut self.stream) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "PostgreSQL socket I/O timeout"));
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

fn frame_error(error: FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid PostgreSQL frame: {error:?}"))
}

fn format_startup_error(error: StartupParameterError) -> String {
    format!("invalid PostgreSQL startup parameters: {error:?}")
}
