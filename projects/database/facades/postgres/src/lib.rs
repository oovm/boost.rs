//! PostgreSQL product facade.
//!
//! Owns the product assembly for the family's PostgreSQL capability: a local
//! engine over the durable KV plane ([`database_storage::cluster_kv::FileShard`]),
//! the SQL semantic source ([`database_query`] with [`SqlDialect::Postgres`]),
//! the protocol-v3 wire gateway ([`database_server::yyds::pgsql::connection`])
//! and the remote client re-export ([`PostgreSQLClient`]).
//!
//! Semantics live in exactly one place: the direct statement path and the wire
//! path both run through the gateway's shared bind → plan → execute core, so a
//! local `query` and a `psql`/`sqlx` client observe identical behavior.
//!
//! Boundaries that remain explicit upstream work (rejected here, never faked):
//! * SQL schema persistence: DDL applies to the in-memory registry and is not
//!   written to the `.yyds` catalog yet. Row data is durable; replaying the
//!   schema DDL on open is the current bootstrap contract for products.
//! * Server-side authentication and TLS: `listen` accepts loopback only and an
//!   `authentication` request fails explicitly until the gateway implements it.

use std::{
    collections::HashMap,
    error,
    fmt,
    io,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use database_client::pgsql::{Connection, ServerStore, error::PostgresError};
use database_cluster::topology::ShardMap;
use database_execution::cluster::LocalExecutor;
use database_query::{SqlDialect, SqlSchemaRegistry, SqlSessionCommand, SqlTransactionAction, parse_ddl, parse_session_command};
use database_server::yyds::pgsql::connection::{
    PgsqlSqlRuntime, TransactionControl, TransactionSession, pg_sqlstate, run_statement, serve_cancellable,
};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

pub use database_query::{SqlColumn, SqlValue};

/// Remote PostgreSQL-compatible client. Construction and semantics stay owned
/// by `database.client`; the facade re-exports it so the product surface has a
/// single entry point.
pub type PostgreSQLClient = Connection;

pub use database_client::pgsql::{Cell, ClientConfig};

/// Failure raised by the facade. `sqlstate` carries the PostgreSQL class shared
/// with the wire path; `Unsupported` marks boundaries the engine rejects
/// explicitly instead of faking success.
#[derive(Debug)]
pub enum Error {
    /// Statement or transaction failure with the PostgreSQL SQLSTATE class.
    Sql {
        /// Five-character PostgreSQL SQLSTATE.
        sqlstate: String,
        /// Human-readable failure detail.
        message: String,
    },
    /// Capability the facade refuses explicitly (auth, savepoints, GUC commands).
    Unsupported(String),
    /// Data directory, storage or transport failure.
    Engine(String),
}

impl Error {
    fn sql(sqlstate: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Sql { sqlstate: sqlstate.into(), message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql { sqlstate, message } => write!(formatter, "{sqlstate}: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::Engine(message) => write!(formatter, "engine failure: {message}"),
        }
    }
}

impl error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Engine(error.to_string())
    }
}

impl From<PostgresError> for Error {
    fn from(error: PostgresError) -> Self {
        Self::Engine(error.to_string())
    }
}

impl From<database_types::cluster::Error> for Error {
    fn from(error: database_types::cluster::Error) -> Self {
        Self::Engine(error.to_string())
    }
}

impl From<database_cluster::topology::routing::RoutingError> for Error {
    fn from(error: database_cluster::topology::routing::RoutingError) -> Self {
        Self::Engine(format!("{error:?}"))
    }
}

fn poisoned() -> Error {
    Error::Engine("postgres facade lock poisoned".to_owned())
}

/// Where the local engine keeps its data directory.
#[derive(Debug, Clone)]
pub struct PostgreSQLStorage {
    /// Data directory holding `postmaster.pid`, the `.yyds` catalog and the `.yykv` shard.
    pub directory: PathBuf,
}

impl Default for PostgreSQLStorage {
    fn default() -> Self {
        Self { directory: PathBuf::from("pgdata") }
    }
}

/// Construction options for [`PostgreSQL`].
#[derive(Debug, Clone)]
pub struct PostgreSQLOptions {
    /// Data directory ownership.
    pub storage: PostgreSQLStorage,
    /// Default database name reported by the product surface. One engine owns
    /// one directory and therefore one database; multi-database hosts open one
    /// engine per directory.
    pub database: String,
}

impl Default for PostgreSQLOptions {
    fn default() -> Self {
        Self { storage: PostgreSQLStorage::default(), database: "postgres".to_owned() }
    }
}

/// Result of one statement on the direct path.
#[derive(Debug)]
pub struct QueryResult {
    /// Result columns; empty when the statement returns no rows.
    pub columns: Vec<SqlColumn>,
    /// Rows in column order.
    pub rows: Vec<Vec<SqlValue>>,
    /// PostgreSQL command tag, e.g. `INSERT 0 3`.
    pub command_tag: String,
}

impl QueryResult {
    fn tag(command_tag: &str) -> Self {
        Self { columns: Vec::new(), rows: Vec::new(), command_tag: command_tag.to_owned() }
    }

    /// Number of rows returned by the statement.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Rows affected by a DML statement, parsed from the command tag.
    pub fn affected_rows(&self) -> Option<u64> {
        self.command_tag.rsplit(' ').next().and_then(|tail| tail.parse().ok())
    }
}

/// Local PostgreSQL engine: owns one data directory and the shared SQL runtime.
pub struct PostgreSQL {
    runtime: Arc<PgsqlSqlRuntime>,
    /// Directory lock and catalog lifecycle. Its internal shard copy stays
    /// unused: the executor mounts the shard file as the single writer, and the
    /// store only holds `postmaster.pid` and the `.yyds` catalog.
    store: Mutex<Option<ServerStore>>,
    session: Mutex<TransactionSession>,
    accept: Mutex<Option<AcceptLoop>>,
    database: String,
}

impl PostgreSQL {
    /// Opens (creating when absent) the data directory described by `options`.
    pub fn open(options: PostgreSQLOptions) -> Result<Self, Error> {
        let store = ServerStore::open(options.storage.directory)?;
        let shard_path = store.shard_path().to_owned();
        let shard_id = ShardId(store.shard_id().to_owned());
        let routing = ShardMap::new(ShardEpoch(1), vec![shard_id.clone()])?;
        let shard = FileShard::open(shard_path)?;
        let executor = LocalExecutor::new(routing, HashMap::from([(shard_id, shard)]), Vec::new())?;
        let runtime = Arc::new(PgsqlSqlRuntime { schema: Arc::new(SqlSchemaRegistry::new_in_memory()), executor: Arc::new(executor) });
        Ok(Self {
            runtime,
            store: Mutex::new(Some(store)),
            session: Mutex::new(TransactionSession::new()),
            accept: Mutex::new(None),
            database: options.database,
        })
    }

    /// Default database name this engine was opened with.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Absolute data directory.
    pub fn data_dir(&self) -> Result<String, Error> {
        Ok(self.lock(&self.store)?.as_ref().ok_or_else(|| Error::Engine("store already closed".to_owned()))?.data_dir())
    }

    /// Runs one statement on the direct path: DDL applies to the shared
    /// registry, transaction control moves the session, everything else runs
    /// through the gateway's bind → plan → execute core.
    pub fn query(&self, sql: &str) -> Result<QueryResult, Error> {
        let mut session = self.lock(&self.session)?;
        self.statement(&mut session, sql)
    }

    /// Alias of [`PostgreSQL::query`] for statements without result rows.
    pub fn execute(&self, sql: &str) -> Result<QueryResult, Error> {
        self.query(sql)
    }

    /// Runs `body` inside one transaction block: `BEGIN`, statements sharing the
    /// session, then `COMMIT` on success or `ROLLBACK` when the body fails.
    pub fn transaction<T>(&self, body: impl FnOnce(&mut Transaction<'_>) -> Result<T, Error>) -> Result<T, Error> {
        let mut session = self.lock(&self.session)?;
        match session.begin(Some(&self.runtime)) {
            TransactionControl::Rejected { code, message } => return Err(Error::sql(code, message)),
            TransactionControl::Done(_) => {}
        }
        let result = {
            let mut transaction = Transaction { engine: self, session: &mut session };
            body(&mut transaction)
        };
        match result {
            Ok(value) => match session.commit(Some(&self.runtime)).map_err(|error| Error::Engine(error.to_string()))? {
                TransactionControl::Done(_) => Ok(value),
                TransactionControl::Rejected { code, message } => Err(Error::sql(code, message)),
            },
            Err(error) => {
                let _ = session.rollback(Some(&self.runtime));
                Err(error)
            }
        }
    }

    /// Starts the protocol-v3 server on a loopback `endpoint` (`port` 0 picks a
    /// free port). The accept loop lives until [`PostgreSQL::close`] or the
    /// next engine teardown; the returned [`Listener`] is the address handle.
    pub fn listen(&self, endpoint: SocketAddr) -> Result<Listener, Error> {
        if !endpoint.ip().is_loopback() {
            return Err(Error::Unsupported(
                "the gateway accepts loopback connections only until authentication and TLS land upstream".to_owned(),
            ));
        }
        let mut slot = self.lock(&self.accept)?;
        if slot.is_some() {
            return Err(Error::Unsupported("this engine already has a live listener".to_owned()));
        }
        let runtime = Arc::clone(&self.runtime);
        let tcp = bind_with_retry(endpoint)?;
        let bound = tcp.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let acceptor = Arc::clone(&stopping);
        let joiner = thread::spawn(move || {
            if tcp.set_nonblocking(true).is_err() {
                return;
            }
            while !acceptor.load(Ordering::Acquire) {
                match tcp.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let conn_runtime = Arc::clone(&runtime);
                        let conn_stopping = Arc::clone(&acceptor);
                        thread::spawn(move || {
                            let _ = serve_cancellable(stream, Arc::clone(&conn_stopping), Some(conn_runtime));
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        });
        *slot = Some(AcceptLoop { stopping, joiner: Mutex::new(Some(joiner)) });
        Ok(Listener { endpoint: bound, database: self.database.clone() })
    }

    /// Stops the listener, releases the directory lock and consumes the engine.
    pub fn close(self) -> Result<(), Error> {
        let accept = self.accept.lock().map_err(|_| poisoned())?.take();
        if let Some(accept) = accept {
            accept.stop();
        }
        let store = self.store.lock().map_err(|_| poisoned())?.take().ok_or_else(|| Error::Engine("store already closed".to_owned()))?;
        store.close().map_err(Error::from)
    }

    /// Shared statement core used by [`PostgreSQL::query`] and by transaction
    /// bodies. Mirrors the wire dispatch order: DDL, session command, then
    /// bind → plan → execute.
    fn statement(&self, session: &mut TransactionSession, sql: &str) -> Result<QueryResult, Error> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Ok(QueryResult::tag(""));
        }
        if session.is_aborted() {
            return Err(Error::sql("25P02", "current transaction is aborted, commands ignored until end of transaction block"));
        }
        match parse_ddl(trimmed, SqlDialect::Postgres) {
            Err(error) => {
                session.fail();
                return Err(Error::sql(pg_sqlstate(error.kind), error.message));
            }
            Ok(Some(statement)) => match self.runtime.schema.apply_ddl(&statement) {
                Ok(outcome) => return Ok(QueryResult::tag(outcome.command_tag())),
                Err(error) => {
                    session.fail();
                    return Err(Error::sql(pg_sqlstate(error.kind), error.message));
                }
            },
            Ok(None) => {}
        }
        match parse_session_command(trimmed) {
            Err(_) => {
                session.fail();
                return Err(Error::sql("42601", "cannot parse SQL session command"));
            }
            Ok(Some(SqlSessionCommand::Transaction(action))) => {
                let control = match action {
                    SqlTransactionAction::Begin => session.begin(Some(&self.runtime)),
                    SqlTransactionAction::Commit => session.commit(Some(&self.runtime)).map_err(|error| Error::Engine(error.to_string()))?,
                    SqlTransactionAction::Rollback => session.rollback(Some(&self.runtime)).map_err(|error| Error::Engine(error.to_string()))?,
                    _ => {
                        session.fail();
                        return Err(Error::sql("0A000", "savepoints are not supported on the direct statement path"));
                    }
                };
                return match control {
                    TransactionControl::Done(tag) => Ok(QueryResult::tag(tag)),
                    TransactionControl::Rejected { code, message } => Err(Error::sql(code, message)),
                };
            }
            Ok(Some(_)) => {
                return Err(Error::Unsupported(
                    "SET / SHOW / DISCARD / RESET are wire-session surfaces; the direct path exposes transaction control and data statements".to_owned(),
                ));
            }
            Ok(None) => {}
        }
        match run_statement(trimmed, &self.runtime, session) {
            Ok(outcome) => Ok(QueryResult { columns: outcome.description, rows: outcome.rows, command_tag: outcome.command_tag }),
            Err((sqlstate, message)) => Err(Error::sql(sqlstate, message)),
        }
    }

    fn lock<'a, T>(&self, slot: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, Error> {
        slot.lock().map_err(|_| poisoned())
    }
}

/// Statements bound to one open transaction block.
pub struct Transaction<'engine> {
    engine: &'engine PostgreSQL,
    session: &'engine mut TransactionSession,
}

impl Transaction<'_> {
    /// Runs one statement inside this transaction block.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult, Error> {
        self.engine.statement(self.session, sql)
    }

    /// Alias of [`Transaction::query`] for statements without result rows.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, Error> {
        self.query(sql)
    }
}

/// Address handle for the engine's live protocol-v3 listener. The accept loop
/// itself is owned by the engine and stopped by [`PostgreSQL::close`].
#[derive(Debug, Clone)]
pub struct Listener {
    endpoint: SocketAddr,
    database: String,
}

impl Listener {
    /// Real bound address, resolving `port` 0.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// Connection string pointing at this listener (`sqlx`-compatible).
    pub fn connection_string(&self) -> String {
        format!("postgres://postgres@{}/{}", self.endpoint, self.database)
    }
}

/// Owned accept loop: cancellation flag plus the joinable accept thread.
struct AcceptLoop {
    stopping: Arc<AtomicBool>,
    joiner: Mutex<Option<JoinHandle<()>>>,
}

impl AcceptLoop {
    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(mut joiner) = self.joiner.lock()
            && let Some(handle) = joiner.take()
        {
            let _ = handle.join();
        }
    }
}

fn bind_with_retry(endpoint: SocketAddr) -> io::Result<TcpListener> {
    let mut delay = Duration::from_millis(10);
    for _ in 0..10 {
        match TcpListener::bind(endpoint) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                thread::sleep(delay);
                delay *= 2;
            }
            Err(error) => return Err(error),
        }
    }
    TcpListener::bind(endpoint)
}

impl database_migration::MigrationTarget for PostgreSQL {
    fn execute(&mut self, sql: &str) -> Result<(), database_migration::Error> {
        PostgreSQL::execute(self, sql).map(|_| ()).map_err(|error| database_migration::Error::Target(error.to_string()))
    }

    fn query(&mut self, sql: &str) -> Result<database_migration::ResultSet, database_migration::Error> {
        let result = PostgreSQL::query(self, sql).map_err(|error| database_migration::Error::Target(error.to_string()))?;
        Ok(database_migration::ResultSet {
            columns: result.columns.into_iter().map(|column| column.name).collect(),
            rows: result.rows.into_iter().map(|row| row.into_iter().map(cell_text).collect()).collect(),
        })
    }
}

/// Renders one stored value the way PostgreSQL's text format spells it, so a
/// migrated row compares equal to the same row read back from the source.
fn cell_text(value: SqlValue) -> Option<String> {
    match value {
        SqlValue::Null => None,
        SqlValue::Integer(number) => Some(number.to_string()),
        SqlValue::Number(number) => Some(number.to_string()),
        SqlValue::String(text) => Some(text),
        SqlValue::Boolean(flag) => Some(if flag { "t".to_owned() } else { "f".to_owned() }),
        SqlValue::Bytes(bytes) => Some(format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
    }
}
