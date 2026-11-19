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
//! * Remote binds: the engine serves loopback only, so a client that needs a
//!   shared host keeps talking to a local gateway.
//!
//! TLS is closed: [`PostgreSQL::listen_secure`] answers `SSLRequest` with `S` and
//! runs the protocol inside the tunnel, `listen`/`listen_with` keep answering `N`
//! so a client without a configured certificate falls back to plaintext.
//!
//! SQL schema persistence is closed: DDL applies to a registry persisted as
//! `<data-dir>/schema.json`, so a restart reads the same tables back.
#![warn(missing_docs)]

use std::{
    collections::HashMap,
    error, fmt, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use database_client::pgsql::{Connection, error::PostgresError};
use database_cluster::topology::ShardMap;
use database_execution::{
    cluster::LocalExecutor,
    pgsql::{PgsqlSqlRuntime, StatementOutcome, TransactionRunError, TransactionSession, dispatch_statement, run_transaction},
    sql_schema::SqlSchemaRegistry,
};
use database_server::yyds::pgsql::{ListenerRegistry, ListenerSlot, PgAuthentication, PgTls, service::PgsqlService};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

pub use database_storage::pgsql::instance;

pub use database_storage::pgsql::instance::{
    SHARD_DIRECTORY_FILES, SHARD_ROOT, ServerInstance, clear_stale_lock, process_exists, shard_directory, validate_shard_id,
};

pub use database_query::{SqlColumn, SqlValue};

/// Authentication a listener requires before statements run.
pub use database_server::yyds::pgsql::PgAuthentication as Authentication;

/// Password method a role-password listener offers.
pub use database_server::yyds::pgsql::PgPasswordMethod as PasswordMethod;

/// Server TLS certificate and key a listener offers on `SSLRequest`.
pub use database_server::yyds::pgsql::PgTls as Tls;

/// Remote PostgreSQL-compatible client. Construction and semantics stay owned
/// by `database.client`; the facade re-exports it so the product surface has a
/// single entry point.
pub type PostgreSQLClient = Connection;

pub use database_client::pgsql::{Cell, ClientConfig};

/// Failure raised by the facade. Structured fields are kept until the FFI or
/// wire boundary, where the caller renders one envelope.
///
/// `Sql::sqlstate` carries the PostgreSQL class shared with the wire path;
/// `Unsupported` marks boundaries the engine rejects explicitly instead of
/// faking success; `Storage` keeps the protocol error class of a data-directory
/// or shard failure instead of flattening it to text.
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
    /// Data-directory, catalog, shard or transport failure of the protocol layer.
    Storage(PostgresError),
    /// Shard routing or topology failure.
    Routing(database_cluster::topology::routing::RoutingError),
    /// Storage-contract failure raised by `database-types`.
    Cluster(database_types::cluster::Error),
    /// Host I/O failure: directory, socket bind or thread lifecycle.
    Io(io::Error),
    /// Engine lifecycle state: a closed store or a poisoned lock.
    State(String),
    /// A transaction block failed after the body, keeping both causes.
    Block {
        /// Failure returned by the body.
        body: Box<Error>,
        /// Failure raised by the compensating `ROLLBACK`.
        control: BlockControl,
    },
}

/// Compensating-control failure after a transaction body failed.
#[derive(Debug)]
pub enum BlockControl {
    /// `ROLLBACK` was rejected with a SQLSTATE.
    Rejected {
        /// SQLSTATE class returned by `ROLLBACK`.
        code: &'static str,
        /// Human-readable rejection detail.
        message: String,
    },
    /// `ROLLBACK` failed at the storage layer.
    Io(io::Error),
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
            Self::Storage(error) => write!(formatter, "storage failure: {error}"),
            Self::Routing(error) => write!(formatter, "routing failure: {error:?}"),
            Self::Cluster(error) => write!(formatter, "cluster failure: {error:?}"),
            Self::Io(error) => write!(formatter, "io failure: {error}"),
            Self::State(message) => write!(formatter, "engine state: {message}"),
            Self::Block { body, control } => match control {
                BlockControl::Rejected { code, message } => {
                    write!(formatter, "transaction body failed: {body}; rollback rejected {code}: {message}")
                }
                BlockControl::Io(error) => write!(formatter, "transaction body failed: {body}; rollback io: {error}"),
            },
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Block { body, .. } => Some(body.as_ref()),
            _ => None,
        }
    }
}

impl From<TransactionRunError<Error>> for Error {
    fn from(error: TransactionRunError<Error>) -> Self {
        match error {
            TransactionRunError::Begin { code, message } => Self::sql(code, message),
            TransactionRunError::Body(error) => error,
            TransactionRunError::Commit { code, message } => Self::sql(code, message),
            TransactionRunError::CommitIo(error) => Self::Io(error),
            TransactionRunError::RollbackRejected { code, message, body } => {
                Self::Block { body: Box::new(body), control: BlockControl::Rejected { code, message } }
            }
            TransactionRunError::RollbackIo { body, error } => Self::Block { body: Box::new(body), control: BlockControl::Io(error) },
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PostgresError> for Error {
    fn from(error: PostgresError) -> Self {
        match error {
            PostgresError::Server { code, message, .. } => Self::sql(code, message),
            other => Self::Storage(other),
        }
    }
}

/// Data-directory storage failures become the protocol class only at this
/// boundary: the wire error text is preserved, but `database.storage` itself no
/// longer describes a local disk or lease failure with a protocol type.
impl From<database_storage::pgsql::Error> for Error {
    fn from(error: database_storage::pgsql::Error) -> Self {
        let mapped = match error {
            database_storage::pgsql::Error::Io(message) => PostgresError::Io(message),
            other => PostgresError::Protocol(other.to_string()),
        };
        Self::Storage(mapped)
    }
}

impl From<database_types::cluster::Error> for Error {
    fn from(error: database_types::cluster::Error) -> Self {
        Self::Cluster(error)
    }
}

impl From<database_cluster::topology::routing::RoutingError> for Error {
    fn from(error: database_cluster::topology::routing::RoutingError) -> Self {
        Self::Routing(error)
    }
}

fn poisoned() -> Error {
    Error::State("postgres facade lock poisoned".to_owned())
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
    /// The single data-directory lease: `postmaster.pid`, the `.yyds` catalog
    /// and the table-group layout. The executor mounts the shard file as the
    /// single writer, so the lease only guards the directory.
    instance: Mutex<Option<ServerInstance>>,
    session: Mutex<TransactionSession>,
    /// One shared slot per live service listener; the slot is the only handle
    /// that can stop the accept loop, shared between the [`Listener`] handle
    /// and [`PostgreSQL::close`].
    /// Live protocol-v3 listeners registered with the server-owned registry.
    listeners: Arc<ListenerRegistry>,
    database: String,
}

impl PostgreSQL {
    /// Opens (creating when absent) the data directory described by `options`.
    /// A `postmaster.pid` whose holder process is gone is reclaimed first.
    pub fn open(options: PostgreSQLOptions) -> Result<Self, Error> {
        let instance = ServerInstance::open(&options.storage.directory)?;
        let shard_path = instance.shard_path();
        let shard_id = ShardId(instance.shard_id().to_owned());
        let routing = ShardMap::new(ShardEpoch(1), vec![shard_id.clone()])?;
        let shard = FileShard::open(shard_path)?;
        let executor = LocalExecutor::new(routing, HashMap::from([(shard_id, shard)]), Vec::new())?;
        // The schema registry is durable inside the data directory: DDL applied
        // by one run is read back by the next one instead of asking the caller
        // to replay it.
        let schema = SqlSchemaRegistry::open(instance.schema_path()).map_err(|error| Error::State(error.message))?;
        // Databases, roles and grants are cluster objects of the same data
        // directory, so a restart sees the same catalog.
        let objects = database_storage::pgsql::PgObjectCatalog::open(options.storage.directory.join(database_storage::pgsql::OBJECTS_FILE))
            .map_err(|error| Error::State(error.message()))?;
        let runtime = Arc::new(PgsqlSqlRuntime::with_objects(Arc::new(schema), Arc::new(executor), Arc::new(objects)));
        Ok(Self {
            runtime,
            instance: Mutex::new(Some(instance)),
            session: Mutex::new(TransactionSession::new()),
            listeners: Arc::new(ListenerRegistry::new()),
            database: options.database,
        })
    }

    /// Default database name this engine was opened with.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Absolute data directory.
    pub fn data_dir(&self) -> Result<String, Error> {
        Ok(self.lock(&self.instance)?.as_ref().ok_or_else(|| Error::State("store already closed".to_owned()))?.data_dir())
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
        run_transaction(&mut session, Some(&self.runtime), |session| {
            let mut transaction = Transaction { engine: self, session };
            body(&mut transaction)
        })
        .map_err(Error::from)
    }

    /// Starts the protocol-v3 server on a loopback `endpoint` (`port` 0 picks a
    /// free port). One engine may serve several listeners; each [`Listener`]
    /// stops its own accept loop without closing the engine.
    pub fn listen(&self, endpoint: SocketAddr) -> Result<Listener, Error> {
        self.listen_with(endpoint, PgAuthentication::Trust)
    }

    /// Starts a listener that authenticates every client with `auth` before any
    /// statement runs.
    pub fn listen_with(&self, endpoint: SocketAddr, auth: PgAuthentication) -> Result<Listener, Error> {
        self.listen_inner(endpoint, auth, None)
    }

    /// Starts a listener that offers `tls` to a client's `SSLRequest`.
    pub fn listen_secure(&self, endpoint: SocketAddr, auth: PgAuthentication, tls: PgTls) -> Result<Listener, Error> {
        self.listen_inner(endpoint, auth, Some(Arc::new(tls)))
    }

    fn listen_inner(&self, endpoint: SocketAddr, auth: PgAuthentication, tls: Option<Arc<PgTls>>) -> Result<Listener, Error> {
        if !endpoint.ip().is_loopback() {
            return Err(Error::Unsupported("the gateway accepts loopback connections only".to_owned()));
        }
        let service = PgsqlService::start_with_runtime(Arc::clone(&self.runtime), endpoint, auth, tls)?;
        let bound = service.address();
        let slot = self.listeners.register(service);
        Ok(Listener { endpoint: bound, database: self.database.clone(), slot, registry: Arc::clone(&self.listeners) })
    }

    /// Stops every listener, releases the directory lock and consumes the engine.
    ///
    /// A listener that fails to stop is reported after the directory lease is
    /// released: the two resources are independent, and the caller must see the
    /// stop failure instead of a silent success.
    pub fn close(self) -> Result<(), Error> {
        let stop_error = self.listeners.stop_all().err().map(Error::Io);
        let instance = self.instance.lock().map_err(|_| poisoned())?.take().ok_or_else(|| Error::State("store already closed".to_owned()))?;
        let closed = instance.close().map_err(Error::from);
        match (stop_error, closed) {
            (None, closed) => closed,
            (Some(stop), Ok(())) => Err(stop),
            (Some(stop), Err(close)) => Err(Error::State(format!("listener stop failed: {stop}; directory close failed: {close}"))),
        }
    }

    /// Shared statement core used by [`PostgreSQL::query`] and by transaction
    /// bodies. `database.execution::pgsql` owns the DDL, transaction and
    /// bind → plan → execute order, so the local path and the wire gateway run
    /// the same session semantics.
    fn statement(&self, session: &mut TransactionSession, sql: &str) -> Result<QueryResult, Error> {
        match dispatch_statement(sql, Some(&self.runtime), session) {
            Ok(StatementOutcome::Command(tag)) => Ok(QueryResult::tag(tag)),
            Ok(StatementOutcome::Result(outcome)) => {
                Ok(QueryResult { columns: outcome.description, rows: outcome.rows, command_tag: outcome.command_tag })
            }
            Ok(StatementOutcome::Session(_)) => Err(Error::Unsupported(
                "SET / SHOW / DISCARD / RESET are wire-session surfaces, the direct path exposes transaction control and data statements"
                    .to_owned(),
            )),
            Err(error) => Err(Error::sql(error.sqlstate, error.message)),
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

/// Address handle for one of the engine's live protocol-v3 listeners.
/// Dropping the handle keeps the listener running; [`PostgreSQL::close`] stops
/// every listener the engine still owns.
pub struct Listener {
    endpoint: SocketAddr,
    database: String,
    /// The service slot this handle stops; shared with the engine registry so
    /// exactly one of [`Listener::close`] / [`PostgreSQL::close`] joins the
    /// accept loop.
    slot: ListenerSlot,
    registry: Arc<ListenerRegistry>,
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

    /// Stops this listener's accept loop. The engine stays open.
    ///
    /// The stop outcome of the shared server resource is returned instead of
    /// being discarded, so a failed join is observable to the caller.
    pub fn close(self) -> Result<(), Error> {
        self.registry.close(&self.slot).map_err(Error::Io)
    }
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
