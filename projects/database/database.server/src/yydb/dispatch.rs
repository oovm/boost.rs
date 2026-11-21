//! Request dispatch for `yydb serve`: wire frames mapped onto a [`ServeBackend`].
//!
//! Kept out of `database.protocol.yy` because it executes database operations.

use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use database_protocol_yy::{
    Frame, MessageType, decode_authenticate, decode_execute_prepared, decode_kv_get, decode_kv_put, decode_micro_register, decode_params,
    decode_prepare, decode_savepoint_name, decode_scalar_call, decode_schema_ensure, encode_authenticate_ok, encode_execute_prepared_ok,
    encode_kv_get_ok, encode_named_execute_prepared_ok, encode_named_rows, encode_prepare_ok, encode_rows, encode_scalar_call_ok,
    encode_schema_ok, error_frame,
};
use database_query::local::{PreparedStatement, QueryKind, SessionOp};
use database_types::{Error, Result};

use super::{
    auth::{AuthBootstrap, Permission, Principal, Session, SessionRegistry},
    session::ServeBackend,
};

// ─── Per-connection session state ─────────────────────────────────────────

/// A prepared statement stored by `dispatch` on the server side.
#[derive(Clone)]
pub struct PreparedPlan {
    /// The lowered plan with full op tree + parameter descriptors.
    pub statement: PreparedStatement,
    /// Connection that owns this plan (for authz auditing).
    pub connection_id: u64,
}

/// Per-connection state — tracks prepared statements, attached [`Session`],
/// and a handle to the bootstrap registry shared with all peers.
pub struct SessionState {
    /// Monotonic id for this connection.
    pub connection_id: u64,
    /// Shared bootstrap registry — every connection Authenticates against it.
    pub registry: std::sync::Arc<SessionRegistry>,
    /// Whether this peer connected from a non-loopback address. Remote
    /// peers may not authenticate as [`PrincipalKind::Anonymous`].
    pub remote: bool,
    /// Attached session after a successful Authenticate handshake.
    pub session: Mutex<Option<Session>>,
    /// Plans prepared on this connection, keyed by `statement_id`.
    pub prepared: Mutex<BTreeMap<String, PreparedPlan>>,
    /// Set after an explicit `SessionClose`; the serve loop stops reading and
    /// lets the connection handler release its transport.
    closing: AtomicBool,
}

impl SessionState {
    /// Fresh per-connection state — no session, empty plan registry, but
    /// already wired to the server's shared [`SessionRegistry`].
    pub fn new(connection_id: u64, registry: std::sync::Arc<SessionRegistry>, remote: bool) -> Self {
        Self {
            connection_id,
            registry,
            remote,
            session: Mutex::new(None),
            prepared: Mutex::new(BTreeMap::new()),
            closing: AtomicBool::new(false),
        }
    }

    /// Whether the peer explicitly closed the session.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Convenience constructor for unit tests — loopback-only anonymous
    /// bootstrap pre-configured with the former default token, and the
    /// session field already populated so `dispatch_authed` paths run
    /// without an on-wire Authenticate frame. This is the 1-argument
    /// replacement for the prior `SessionState::new(id)` used across
    /// in-tree tests.
    pub fn for_test(connection_id: u64) -> Self {
        Self::build_for_test(connection_id, true)
    }

    /// Same bootstrap as [`Self::for_test`] but leaves the session field
    /// `None` so callers can run a real `Authenticate` frame through
    /// `dispatch` and observe the handshake path.
    pub fn for_test_unauthed(connection_id: u64) -> Self {
        Self::build_for_test(connection_id, false)
    }

    fn build_for_test(connection_id: u64, pre_auth: bool) -> Self {
        let token = std::env::var("YYDB_AUTH_TOKEN").unwrap_or_else(|_| "yydb-phase0-default-token".to_owned());
        let principal = Principal::anonymous_loopback();
        let bootstrap = AuthBootstrap { tokens: vec![(token.clone(), principal.clone())] };
        let registry = std::sync::Arc::new(SessionRegistry::new(&bootstrap));
        let session = if pre_auth {
            let now = std::time::SystemTime::now();
            Some(Session { principal, token, created_at: now, expires_at: now + Session::TTL })
        }
        else {
            None
        };
        Self {
            connection_id,
            registry,
            remote: false,
            session: Mutex::new(session),
            prepared: Mutex::new(BTreeMap::new()),
            closing: AtomicBool::new(false),
        }
    }
}

/// Read auth status for a connection (serve.rs needs this to drop the
/// handshake timeout once the client has a real session attached).
pub fn is_authenticated(state: &SessionState) -> bool {
    state.session.lock().expect("session auth poisoned").is_some()
}

/// 解码 ExecutePrepared 的原生参数体为具名参数表。
fn parse_params(body: &[u8]) -> Result<BTreeMap<String, database_types::Value>> {
    decode_params(body)
}

// ─── Dispatch ────────────────────────────────────────────────────────────

/// Dispatch one client request frame against a [`ServeBackend`], with
/// per-connection session enforcement via the shared [`SessionRegistry`].
///
/// Connection lifecycle:
/// 1. `Hello` — always accepted (no auth required)
/// 2. `Authenticate` — verify token against bootstrap registry + bind-policy
///    rules, attach a [`Session`] on success
/// 3. Every other request — rejected with structured auth error until a
///    session is attached
///
/// Once authenticated, Prepare / ExecutePrepared hit the per-connection
/// plan registry; all other frames dispatch directly to the backend.
pub fn dispatch<B: ServeBackend + ?Sized>(backend: &B, state: &SessionState, request: &Frame) -> Frame {
    let id = request.request_id;
    match request.msg_type {
        // ── unauthenticated allowed ──────────────────────────────────────
        MessageType::Hello => Frame::new(MessageType::HelloOk, id, env!("CARGO_PKG_VERSION").as_bytes().to_vec()),
        MessageType::Authenticate => {
            let token = match decode_authenticate(&request.body) {
                Ok(t) => t,
                Err(error) => return error_frame(id, &error),
            };
            match state.registry.lookup_session(&token, state.remote) {
                Ok(session) => {
                    *state.session.lock().unwrap() = Some(session.clone());
                    Frame::new(MessageType::AuthenticateOk, id, encode_authenticate_ok(&session.token))
                }
                Err(error) => error_frame(id, &error),
            }
        }
        // ── authenticated required ──────────────────────────────────────
        _ => {
            if !is_authenticated(state) {
                return error_frame(
                    id,
                    &Error::AuthTokenInvalid {
                        message: format!("connection not authenticated — send Hello + Authenticate first (got {})", request.msg_type.as_u16()),
                    },
                );
            }
            dispatch_authed(backend, state, request)
        }
    }
}

fn dispatch_authed<B: ServeBackend + ?Sized>(backend: &B, state: &SessionState, request: &Frame) -> Frame {
    let id = request.request_id;

    // ── Permission gate ──────────────────────────────────────────────
    {
        let session_guard = state.session.lock().expect("session auth poisoned");
        let session = session_guard.as_ref().expect("auth check above guarantees session present");
        let required = required_permission(request.msg_type);
        if !session.principal.has_permission(required) {
            return error_frame(
                id,
                &Error::AuthInsufficientPermission { required: required.as_str().to_owned(), frame: format!("{:?}", request.msg_type) },
            );
        }
    }

    match request.msg_type {
        MessageType::Info => match backend.info() {
            Ok(body) => Frame::new(MessageType::InfoOk, id, body),
            Err(error) => error_frame(id, &error),
        },
        MessageType::SchemaGet => match backend.schema() {
            Ok(schema) => Frame::new(MessageType::SchemaGetOk, id, encode_schema_ok(schema)),
            Err(error) => error_frame(id, &error),
        },
        MessageType::SchemaEnsure => match decode_schema_ensure(&request.body) {
            Ok((version, document)) => match backend.ensure_schema(version, &document) {
                Ok(()) => Frame::new(MessageType::SchemaEnsureOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::KvGet => match decode_kv_get(&request.body) {
            Ok(key) => match backend.kv_get(&key) {
                Ok(value) => Frame::new(MessageType::KvGetOk, id, encode_kv_get_ok(value)),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::KvPut => match decode_kv_put(&request.body) {
            Ok((key, value)) => match backend.kv_put(key, value) {
                Ok(()) => Frame::new(MessageType::KvPutOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::MicroRegister => match decode_micro_register(&request.body) {
            Ok(definition) => match backend.register_host_micro(definition) {
                Ok(()) => Frame::new(MessageType::MicroRegisterOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::ScalarCall => match decode_scalar_call(&request.body) {
            Ok((name, version, args)) => match backend.call_scalar_version(&name, version, &args) {
                Ok(value) => Frame::new(MessageType::ScalarCallOk, id, encode_scalar_call_ok(value)),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        // ── Prepared statement path ──────────────────────────────────────
        MessageType::Prepare => {
            let source = match decode_prepare(&request.body) {
                Ok(s) => s,
                Err(error) => return error_frame(id, &error),
            };
            let stmt = match backend.prepare(&source) {
                Ok(s) => s,
                Err(error) => return error_frame(id, &error),
            };
            let sid = stmt.statement_id.clone();
            state.prepared.lock().unwrap().insert(sid.clone(), PreparedPlan { statement: stmt, connection_id: state.connection_id });
            Frame::new(MessageType::PrepareOk, id, encode_prepare_ok(&sid))
        }
        MessageType::ExecutePrepared => {
            let (sid, params_body) = match decode_execute_prepared(&request.body) {
                Ok(v) => v,
                Err(error) => return error_frame(id, &error),
            };
            let plan = match state.prepared.lock().unwrap().get(&sid).cloned() {
                Some(p) => p,
                None => {
                    return error_frame(
                        id,
                        &Error::QueryBind {
                            parameter: String::new(),
                            message: format!("unknown prepared statement `{sid}` — must Prepare first on this connection"),
                        },
                    );
                }
            };

            // ── Dynamic gate by statement effect kind ──────────────────
            // Read-plan → already covered by the upfront `Permission::Read`
            // gate above; write-plan → must also hold `Permission::Write`.
            let required = match plan.statement.query_kind {
                QueryKind::Read => Permission::Read,
                QueryKind::Write => Permission::Write,
                QueryKind::Session => Permission::Write,
            };
            {
                let session_guard = state.session.lock().expect("session auth poisoned");
                let session = session_guard.as_ref().expect("auth check above guarantees session present");
                if !session.principal.has_permission(required) {
                    return error_frame(
                        id,
                        &Error::AuthInsufficientPermission {
                            required: required.as_str().to_owned(),
                            frame: format!("ExecutePrepared({})", sid),
                        },
                    );
                }
            }

            let named_params = match params_body {
                Some(body) => match parse_params(&body) {
                    Ok(m) => m,
                    Err(error) => return error_frame(id, &error),
                },
                None => BTreeMap::new(),
            };

            // ── Route Session ops directly onto the connection boundary ──
            // `query_prepared` inside the engine only drives the read pipeline.
            // Session / transaction boundary ops must go straight through the
            // connection surface so savepoint state and txn bookkeeping see them.
            let rows_result: Result<Vec<database_query::local::QueryRow>> = match plan.statement.query_kind {
                QueryKind::Session => (|| -> Result<Vec<database_query::local::QueryRow>> {
                    for op in &plan.statement.session_ops {
                        match op {
                            SessionOp::Begin => backend.begin()?,
                            SessionOp::Commit => backend.commit()?,
                            SessionOp::Rollback => backend.rollback()?,
                            SessionOp::Savepoint(name) => backend.savepoint(name)?,
                            SessionOp::RollbackToSavepoint(name) => backend.rollback_to_savepoint(name)?,
                            SessionOp::ReleaseSavepoint(name) => backend.release_savepoint(name)?,
                        }
                    }
                    Ok(Vec::new())
                })(),
                // Prepared writes must not enter the read pipeline: a VOS write
                // plan carries no read ops and would silently affect zero rows.
                QueryKind::Write => backend.execute_prepared(&plan.statement, &named_params),
                _ => backend.query_prepared(&plan.statement, &named_params),
            };
            let rows = match rows_result {
                Ok(r) => r,
                Err(error) => return error_frame(id, &error),
            };
            execute_ok_frame(backend, id, &rows)
        }
        // ── Transaction boundary ─────────────────────────────────────────
        MessageType::TxnBegin => match backend.begin() {
            Ok(()) => Frame::new(MessageType::TxnBeginOk, id, Vec::new()),
            Err(error) => error_frame(id, &error),
        },
        MessageType::TxnCommit => match backend.commit() {
            Ok(()) => Frame::new(MessageType::TxnCommitOk, id, Vec::new()),
            Err(error) => error_frame(id, &error),
        },
        MessageType::TxnRollback => match backend.rollback() {
            Ok(()) => Frame::new(MessageType::TxnRollbackOk, id, Vec::new()),
            Err(error) => error_frame(id, &error),
        },
        MessageType::TxnSavepoint => match decode_savepoint_name(&request.body) {
            Ok(name) => match backend.savepoint(&name) {
                Ok(()) => Frame::new(MessageType::TxnSavepointOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::TxnRollbackTo => match decode_savepoint_name(&request.body) {
            Ok(name) => match backend.rollback_to_savepoint(&name) {
                Ok(()) => Frame::new(MessageType::TxnRollbackToOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::TxnRelease => match decode_savepoint_name(&request.body) {
            Ok(name) => match backend.release_savepoint(&name) {
                Ok(()) => Frame::new(MessageType::TxnReleaseOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        MessageType::PerfReport => match backend.perf_report() {
            Ok(report) => Frame::new(MessageType::PerfReportOk, id, report.into_bytes()),
            Err(error) => error_frame(id, &error),
        },
        // ── Explicit session close: release the lease before the socket dies ──
        MessageType::SessionClose => {
            let outcome = backend.close();
            let token = state.session.lock().expect("session auth poisoned").as_ref().map(|session| session.token.clone());
            if let Some(token) = token {
                state.registry.release_session(&token);
            }
            *state.session.lock().expect("session auth poisoned") = None;
            state.prepared.lock().expect("prepared registry poisoned").clear();
            state.closing.store(true, Ordering::Release);
            match outcome {
                Ok(()) => Frame::new(MessageType::SessionCloseOk, id, Vec::new()),
                Err(error) => error_frame(id, &error),
            }
        }
        // ── server→client messages / handshake-only frames we never dispatch here ──
        MessageType::HelloOk
        | MessageType::AuthenticateOk
        | MessageType::InfoOk
        | MessageType::SchemaGetOk
        | MessageType::SchemaEnsureOk
        | MessageType::KvGetOk
        | MessageType::KvPutOk
        | MessageType::MicroRegisterOk
        | MessageType::MicroHostInvoke
        | MessageType::MicroHostInvokeOk
        | MessageType::ScalarCallOk
        | MessageType::PrepareOk
        | MessageType::ExecutePreparedOk
        | MessageType::TxnBeginOk
        | MessageType::TxnCommitOk
        | MessageType::TxnRollbackOk
        | MessageType::TxnSavepointOk
        | MessageType::TxnRollbackToOk
        | MessageType::TxnReleaseOk
        | MessageType::PerfReportOk
        | MessageType::SessionCloseOk
        | MessageType::Error => error_frame(id, &Error::Protocol { message: "unexpected client message type".to_owned() }),
        // Hello / Authenticate are handled by dispatch() before auth check — reaching here means
        // they somehow slipped through; reject defensively.
        MessageType::Hello | MessageType::Authenticate => {
            error_frame(id, &Error::Protocol { message: "handshake frame received after auth — protocol violation".to_owned() })
        }
        MessageType::Unknown(code) => error_frame(id, &Error::Protocol { message: format!("unknown msg_type {code}") }),
    }
}

/// Build the `ExecutePreparedOk` reply for one result set.
///
/// The runtime directory decides the projection: a schema that declares named
/// types sends the named wire projection together with its directory, so the
/// client validates revisions and discriminants without holding a schema
/// contract. A schema without named types keeps the plain row encoding.
fn execute_ok_frame<B: ServeBackend + ?Sized>(backend: &B, id: u32, rows: &[database_query::local::QueryRow]) -> Frame {
    let directory = match backend.wire_named_directory() {
        Ok(directory) => directory,
        Err(error) => return error_frame(id, &error),
    };
    match directory {
        Some(directory) => match encode_named_rows(rows, &directory) {
            Ok(rows_body) => match encode_named_execute_prepared_ok(&directory, &rows_body) {
                Ok(body) => Frame::new(MessageType::ExecutePreparedOk, id, body),
                Err(error) => error_frame(id, &error),
            },
            Err(error) => error_frame(id, &error),
        },
        None => match encode_rows(rows) {
            Ok(rows_body) => Frame::new(MessageType::ExecutePreparedOk, id, encode_execute_prepared_ok(&rows_body)),
            Err(error) => error_frame(id, &error),
        },
    }
}

/// Which [`Permission`] an incoming frame requires before dispatch.
///
/// * `KvPut` → [`Permission::Write`]
/// * `SchemaEnsure` → [`Permission::Schema`]
/// * Everything else (reads, prepared statements, UDF calls) → [`Permission::Read`]
fn required_permission(msg: MessageType) -> Permission {
    match msg {
        MessageType::KvPut => Permission::Write,
        MessageType::SchemaEnsure => Permission::Schema,
        MessageType::TxnBegin
        | MessageType::TxnCommit
        | MessageType::TxnRollback
        | MessageType::TxnSavepoint
        | MessageType::TxnRollbackTo
        | MessageType::TxnRelease => Permission::Write,
        _ => Permission::Read,
    }
}

/// Whether a `--bind` host is loopback (product-safe default).
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "::1" | "localhost" | "0:0:0:0:0:0:0:1")
}
