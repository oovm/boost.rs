//! PostgreSQL session state: transaction blocks, the schema checkpoint and the
//! emulated GUCs the wire session reports.
//!
//! The session is execution state, not protocol state: the local engine and the
//! wire gateway both open a [`TransactionSession`] and run every statement
//! through [`crate::pgsql::dispatch_statement`], so block entry, schema
//! checkpointing and aborted-block rejection exist once.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::Duration,
};

use crate::cluster::{KeyValueExecutor, TransactionHandle};
use database_query::{OnCommit, SqlCatalog, SqlColumn, SqlDdlStatement, SqlSchemaCheckpoint, SqlSchemaRegistry, SqlValue};

/// Type-identity lane temporary tables own, so a session-local table never
/// shares a row namespace with the persistent PostgreSQL lane.
const TEMP_TYPE_ID_LANE: u8 = 3;

/// Session-local temporary tables.
///
/// They live in their own in-memory registry, so `CREATE TEMP TABLE` never
/// touches the persisted schema document and another connection cannot see the
/// table. Dropping the registry when the connection closes is the whole
/// cleanup step: the rows already written stay in the key/value store exactly
/// like the rows of a dropped persistent table.
struct TempTables {
    registry: SqlSchemaRegistry,
    on_commit: BTreeMap<String, OnCommit>,
}

impl TempTables {
    fn new() -> Self {
        Self { registry: SqlSchemaRegistry::new_in_memory_in_lane(TEMP_TYPE_ID_LANE), on_commit: BTreeMap::new() }
    }
}

/// One SQL-level prepared statement registered through `PREPARE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSqlStatement {
    /// Statement text that follows `AS`.
    pub sql: String,
    /// Declared parameter types, as written.
    pub parameter_types: Vec<String>,
}

/// One materialized cursor declared through `DECLARE`.
///
/// The gateway has no streaming cursor: `DECLARE` runs the query once and keeps
/// its rows, so `FETCH` only slices that result. `with_hold` rows survive a
/// `COMMIT`, matching PostgreSQL's holdable cursor rule.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCursor {
    /// Query text the cursor materialized.
    pub sql: String,
    /// Result shape.
    pub description: Vec<SqlColumn>,
    /// Materialized rows.
    pub rows: Vec<Vec<SqlValue>>,
    /// Index of the next row `FETCH NEXT` returns.
    pub position: usize,
    /// Whether `WITH HOLD` was written.
    pub with_hold: bool,
}

/// PostgreSQL's default `search_path`, also the value `RESET search_path` and
/// `DISCARD ALL` restore. `SHOW search_path` prints it exactly as the engine
/// does, quotes included, because `$user` must be quoted to stay an identifier.
pub const DEFAULT_SEARCH_PATH: &str = "\"$user\", public";

/// SQLSTATE `25P02` message for statements issued inside an aborted block.
pub const ABORTED_BLOCK: &str = "current transaction is aborted, commands ignored until end of transaction block";

/// Outcome of the shared transaction-block control used by the wire session
/// commands and by the local engine's direct path.
#[derive(Debug)]
pub enum TransactionControl {
    /// The block transition completed; carries the PostgreSQL command tag.
    Done(&'static str),
    /// The command was rejected; carries SQLSTATE and message.
    Rejected {
        /// SQLSTATE class returned to the client.
        code: &'static str,
        /// Human-readable failure detail.
        message: String,
    },
}

/// PostgreSQL SQL execution resources: the local schema registry and the KV
/// execution plane.
#[derive(Debug)]
pub struct PgsqlSqlRuntime {
    /// Local schema registry holding catalog snapshots and applying DDL.
    pub schema: Arc<SqlSchemaRegistry>,
    /// KV execution plane.
    pub executor: Arc<dyn KeyValueExecutor>,
    /// Listener/notify fan-out shared by every session of this runtime.
    pub notifications: Arc<super::notify::NotificationHub>,
}

impl PgsqlSqlRuntime {
    /// Builds a runtime with an empty notification hub.
    pub fn new(schema: Arc<SqlSchemaRegistry>, executor: Arc<dyn KeyValueExecutor>) -> Self {
        Self { schema, executor, notifications: Arc::new(super::notify::NotificationHub::new()) }
    }
}

/// Session-scoped transaction state: the PostgreSQL wire protocol ReadyForQuery
/// byte plus a real storage transaction handle that writes and row reads are
/// bound to. Also holds session-scoped GUC overrides that `SET` mutates and
/// `SHOW` queries.
pub struct TransactionSession {
    /// The executor Arc stays alive for as long as a transaction is open so the handle's
    /// internal references remain valid.
    executor: Option<Arc<dyn KeyValueExecutor>>,
    handle: Option<Box<dyn TransactionHandle + 'static>>,
    /// Tracks whether we are inside a transaction block even without a runtime handle
    /// (e.g. the PostgreSQL protocol test).
    protocol_only_transaction: bool,
    failed: bool,
    /// `search_path` as last set via `SET search_path = ...`. Defaults to the
    /// PostgreSQL convention `"$user, public"`.
    search_path: String,
    /// Startup `database` parameter, reported by synthesized `current_database()`.
    database: String,
    /// Registry snapshot captured at `BEGIN`. The catalog rides the same
    /// transaction as rows: `COMMIT` keeps it, every rollback path restores it.
    schema_checkpoint: Option<SqlSchemaCheckpoint>,
    /// SQL-level prepared statements owned by this session.
    prepared: BTreeMap<String, PreparedSqlStatement>,
    /// SQL-level cursors owned by this session.
    cursors: BTreeMap<String, SessionCursor>,
    /// Notification hub this session subscribed to, when it ever listened.
    notifications: Option<Arc<super::notify::NotificationHub>>,
    /// Sink id inside [`Self::notifications`].
    listener: Option<u64>,
    /// Session-local temporary tables, created on demand.
    temp: Option<TempTables>,
    /// Temporary-registry snapshot captured at `BEGIN`, so a rollback drops the
    /// temp tables the block created.
    temp_checkpoint: Option<SqlSchemaCheckpoint>,
    /// `statement_timeout` in effect for this session, `None` when disabled.
    statement_timeout: Option<Duration>,
    /// Text form `SHOW statement_timeout` reports back.
    statement_timeout_text: String,
}

impl TransactionSession {
    /// Opens a session with no active transaction and default GUCs.
    pub fn new() -> Self {
        Self {
            executor: None,
            handle: None,
            protocol_only_transaction: false,
            failed: false,
            search_path: DEFAULT_SEARCH_PATH.to_string(),
            database: "postgres".to_string(),
            schema_checkpoint: None,
            prepared: BTreeMap::new(),
            cursors: BTreeMap::new(),
            notifications: None,
            listener: None,
            temp: None,
            temp_checkpoint: None,
            statement_timeout: None,
            statement_timeout_text: "0".to_string(),
        }
    }

    /// Captures the registry state when a transaction block opens.
    fn capture_schema(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) {
        self.schema_checkpoint = runtime.map(|rt| rt.schema.checkpoint());
        self.temp_checkpoint = self.temp.as_ref().map(|temp| temp.registry.checkpoint());
    }

    /// Opens a transaction block. Shared by the wire `BEGIN` path and the local
    /// engine, so block entry, schema checkpointing and the aborted block
    /// rejection live in one place.
    pub fn begin(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) -> TransactionControl {
        if self.failed {
            return TransactionControl::Rejected { code: "25P02", message: ABORTED_BLOCK.to_string() };
        }
        let Some(rt) = runtime
        else {
            // No runtime — stay in protocol-only mode. `execute_plan` receives
            // `None` for the handle and goes through the executor directly.
            self.executor = None;
            self.handle = None;
            self.protocol_only_transaction = true;
            self.failed = false;
            self.schema_checkpoint = None;
            return TransactionControl::Done("BEGIN");
        };
        // The executor owns the transaction contract: `begin_transaction` captures
        // the ownership context (single-node gateways pass `None` explicitly) and
        // the returned handle replays it at the commit boundary.
        match Arc::clone(&rt.executor).begin_transaction(None) {
            Ok(handle) => {
                self.executor = Some(rt.executor.clone());
                self.handle = Some(handle);
                self.protocol_only_transaction = false;
                self.failed = false;
                self.capture_schema(Some(rt));
                TransactionControl::Done("BEGIN")
            }
            Err(err) => TransactionControl::Rejected { code: "0A000", message: format!("BEGIN failed: {err}") },
        }
    }

    /// Ends a transaction block. A failed commit discards the collected DDL;
    /// committing an aborted block rolls it back, matching the wire semantics.
    pub fn commit(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) -> io::Result<TransactionControl> {
        if self.failed {
            if let Some(handle) = self.handle.take() {
                handle.rollback();
            }
            self.executor = None;
            self.handle = None;
            self.protocol_only_transaction = false;
            self.failed = false;
            self.drop_cursors(false);
            // `COMMIT` of an aborted block is a rollback: the catalog goes with the rows.
            self.restore_schema(runtime)?;
            self.restore_temp();
            return Ok(TransactionControl::Done("ROLLBACK"));
        }
        if let Some(handle) = self.handle.take() {
            if let Err(err) = handle.commit() {
                let code = match &err {
                    database_types::cluster::Error::CasConflict { .. } => "40001",
                    _ => "58P01",
                };
                // A failed commit did not publish the transaction, so the
                // DDL it collected is discarded too.
                self.restore_schema(runtime)?;
                return Ok(TransactionControl::Rejected { code, message: format!("COMMIT failed: {err}") });
            }
        }
        self.executor = None;
        self.handle = None;
        self.protocol_only_transaction = false;
        self.failed = false;
        // Holdable cursors survive the commit, everything else closes.
        self.drop_cursors(true);
        // The commit point publishes the schema revision as well, and applies
        // the temporary tables' `ON COMMIT` actions.
        self.schema_checkpoint = None;
        self.apply_on_commit();
        self.temp_checkpoint = None;
        Ok(TransactionControl::Done("COMMIT"))
    }

    /// Aborts a transaction block, restoring the schema captured at `BEGIN`.
    pub fn rollback(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) -> io::Result<TransactionControl> {
        if let Some(handle) = self.handle.take() {
            handle.rollback();
        }
        self.executor = None;
        self.handle = None;
        self.protocol_only_transaction = false;
        self.failed = false;
        self.drop_cursors(false);
        self.restore_schema(runtime)?;
        self.restore_temp();
        Ok(TransactionControl::Done("ROLLBACK"))
    }

    /// Restores the snapshot captured at `BEGIN`. Only rollback paths call
    /// this — a successful `COMMIT` drops the checkpoint instead. The wire
    /// session calls it when a connection disappears mid-block.
    pub fn restore_schema(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) -> io::Result<()> {
        if let (Some(checkpoint), Some(rt)) = (self.schema_checkpoint.take(), runtime) {
            rt.schema.restore(checkpoint).map_err(|error| io::Error::other(error.message))?;
        }
        Ok(())
    }

    /// `RESET` / `RESET ALL`: every emulated GUC returns to its default.
    ///
    /// The gateway keeps no override for GUCs other than `search_path` (their
    /// `SET` is accepted as a no-op and `SHOW` answers the default table), so
    /// restoring them is already satisfied by the static default table.
    pub fn reset_parameters(&mut self) {
        self.search_path = DEFAULT_SEARCH_PATH.to_string();
        self.statement_timeout = None;
        self.statement_timeout_text = "0".to_string();
    }

    /// Applies a `SET statement_timeout = ...` value.
    ///
    /// PostgreSQL accepts an integer number of milliseconds or a text value with
    /// a `ms` / `s` / `min` unit; `0` disables the limit. Returns whether the
    /// value was understood, so the caller can report `22023` for junk instead
    /// of silently accepting it.
    pub fn set_statement_timeout(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        let lower = trimmed.to_ascii_lowercase();
        let (digits, millis_per_unit) = if let Some(value) = lower.strip_suffix("ms") {
            (value, 1u64)
        }
        else if let Some(value) = lower.strip_suffix("min") {
            (value, 60_000)
        }
        else if let Some(value) = lower.strip_suffix('s') {
            (value, 1_000)
        }
        else {
            (lower.as_str(), 1)
        };
        let Ok(amount) = digits.trim().parse::<u64>()
        else {
            return false;
        };
        let millis = amount.saturating_mul(millis_per_unit);
        self.statement_timeout = (millis > 0).then(|| Duration::from_millis(millis));
        self.statement_timeout_text = if millis == 0 { "0".to_string() } else { format!("{millis}ms") };
        true
    }

    /// Effective per-statement limit, `None` when disabled.
    pub fn statement_timeout(&self) -> Option<Duration> {
        self.statement_timeout
    }

    /// Text `SHOW statement_timeout` reports.
    pub fn statement_timeout_text(&self) -> &str {
        &self.statement_timeout_text
    }

    /// `DISCARD ALL`: full session cleanup. PostgreSQL only lets the statement run
    /// outside a transaction block; the caller rejects the in-block case with
    /// `25001` before this runs, so any transaction state here is a leftover. It
    /// is rolled back — never committed — emulated GUCs return to their defaults
    /// and the aborted flag clears. Prepared statements and portals live in the
    /// wire session and are dropped by its caller.
    pub fn discard_all(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.rollback();
        }
        self.executor = None;
        self.protocol_only_transaction = false;
        self.failed = false;
        self.prepared.clear();
        self.cursors.clear();
        // `DISCARD ALL` is rejected inside a transaction block, so there is no
        // pending checkpoint here; dropping the slot keeps the invariant that
        // only an open block owns schema rollback.
        self.schema_checkpoint = None;
        // PostgreSQL drops the session's temporary tables with `DISCARD ALL`.
        self.temp = None;
        self.temp_checkpoint = None;
        self.reset_parameters();
    }

    /// Marks the open block aborted so later statements report `25P02`.
    pub fn fail(&mut self) {
        if self.handle.is_some() || self.protocol_only_transaction {
            self.failed = true;
        }
    }

    /// Whether the open block is aborted and only `ROLLBACK` may end it.
    pub fn is_aborted(&self) -> bool {
        self.failed
    }

    /// Whether a transaction block is open, runtime-backed or protocol-only.
    pub fn in_transaction(&self) -> bool {
        self.handle.is_some() || self.protocol_only_transaction
    }

    /// ReadyForQuery byte for the current session state (`I` / `T` / `E`).
    pub fn ready_byte(&self) -> u8 {
        if self.failed {
            b'E'
        }
        else if self.in_transaction() {
            b'T'
        }
        else {
            b'I'
        }
    }

    /// Restores the temporary registry captured at `BEGIN`, dropping every
    /// temp table the aborted block created.
    pub fn restore_temp(&mut self) {
        let Some(checkpoint) = self.temp_checkpoint.take()
        else {
            return;
        };
        let Some(temp) = &mut self.temp
        else {
            return;
        };
        if temp.registry.restore(checkpoint).is_ok() {
            let known: BTreeSet<String> = temp.registry.catalog().tables().iter().map(|table| table.name.clone()).collect();
            temp.on_commit.retain(|name, _| known.contains(name));
        }
    }

    /// Applies the `ON COMMIT DROP` actions of every temporary table at the
    /// commit point.
    fn apply_on_commit(&mut self) {
        let Some(temp) = &mut self.temp
        else {
            return;
        };
        let dropped: Vec<String> =
            temp.on_commit.iter().filter(|(_, action)| **action == OnCommit::Drop).map(|(name, _)| name.clone()).collect();
        temp.on_commit.clear();
        if dropped.is_empty() {
            return;
        }
        let statement = SqlDdlStatement::DropTable { names: dropped, if_exists: true };
        let _ = temp.registry.apply_ddl(&statement);
    }

    /// Applies one DDL statement to the session's temporary registry.
    pub fn apply_temp_ddl(&mut self, statement: &SqlDdlStatement) -> Result<database_query::SqlDdlOutcome, database_query::SqlFrontendError> {
        let in_block = self.in_transaction();
        let temp = self.temp.get_or_insert_with(TempTables::new);
        // A block that only creates its first temporary table still needs the
        // snapshot, so the rollback can drop it again.
        if in_block && self.temp_checkpoint.is_none() {
            self.temp_checkpoint = Some(temp.registry.checkpoint());
        }
        let outcome = temp.registry.apply_ddl(statement)?;
        if let SqlDdlStatement::CreateTable { name, on_commit, .. } = statement
            && matches!(outcome, database_query::SqlDdlOutcome::CreateTable { created: true })
        {
            temp.on_commit.insert(name.clone(), *on_commit);
        }
        Ok(outcome)
    }

    /// Whether the session owns a temporary table with this name.
    pub fn temp_table_exists(&self, name: &str) -> bool {
        self.temp.as_ref().is_some_and(|temp| temp.registry.catalog().tables().iter().any(|table| table.name == name))
    }

    /// Registry that owns the value sequences of `type_id`: the temporary one
    /// for a session-local table, the persistent one otherwise.
    pub fn sequence_registry<'a>(&'a self, schema: &'a SqlSchemaRegistry, type_id: u64) -> &'a SqlSchemaRegistry {
        match &self.temp {
            Some(temp) if temp.registry.catalog().tables().iter().any(|table| table.type_id == type_id) => &temp.registry,
            _ => schema,
        }
    }

    /// Catalog this session binds against: its temporary tables first, then the
    /// persistent schema minus any table a temporary one shadows, which is the
    /// `pg_temp` → `public` search-path order PostgreSQL applies.
    pub fn catalog(&self, schema: &SqlSchemaRegistry) -> SqlCatalog {
        let base = schema.catalog();
        let Some(temp) = &self.temp
        else {
            return base;
        };
        let temp_catalog = temp.registry.catalog();
        if temp_catalog.tables().is_empty() {
            return base;
        }
        let shadowed: BTreeSet<&str> = temp_catalog.tables().iter().map(|table| table.name.as_str()).collect();
        let tables = temp_catalog
            .tables()
            .iter()
            .chain(base.tables().iter().filter(|table| !shadowed.contains(table.name.as_str())))
            .cloned()
            .collect();
        SqlCatalog::from_tables(base.schema_version(), tables).unwrap_or(base)
    }

    /// `search_path` as last set through `SET`. `SHOW search_path` and
    /// `current_schema()` read it.
    pub fn search_path(&self) -> &str {
        &self.search_path
    }

    /// Applies one `SET search_path = ...` value.
    pub fn set_search_path(&mut self, path: String) {
        self.search_path = path;
    }

    /// Applies the startup `database` parameter.
    pub fn set_database(&mut self, database: String) {
        self.database = database;
    }

    /// Open transaction handle, when this session owns a runtime transaction.
    pub fn transaction_handle(&self) -> Option<&dyn TransactionHandle> {
        self.handle.as_deref()
    }

    /// Registers a SQL-level prepared statement. Returns `false` when the name
    /// already exists, which PostgreSQL reports as `42P05`.
    pub fn prepare_statement(&mut self, name: &str, statement: PreparedSqlStatement) -> bool {
        if self.prepared.contains_key(name) {
            return false;
        }
        self.prepared.insert(name.to_owned(), statement);
        true
    }

    /// Looks up a SQL-level prepared statement by name.
    pub fn prepared_statement(&self, name: &str) -> Option<&PreparedSqlStatement> {
        self.prepared.get(name)
    }

    /// Drops one prepared statement, or every one when `name` is `None`.
    /// Returns whether anything was removed.
    pub fn deallocate_statement(&mut self, name: Option<&str>) -> bool {
        match name {
            None => {
                let had_any = !self.prepared.is_empty();
                self.prepared.clear();
                had_any
            }
            Some(name) => self.prepared.remove(name).is_some(),
        }
    }

    /// Registers a declared cursor. Returns `false` when the name is taken,
    /// which PostgreSQL reports as `42P03`.
    pub fn declare_cursor(&mut self, name: &str, cursor: SessionCursor) -> bool {
        if self.cursors.contains_key(name) {
            return false;
        }
        self.cursors.insert(name.to_owned(), cursor);
        true
    }

    /// Looks up a cursor by name.
    pub fn cursor(&self, name: &str) -> Option<&SessionCursor> {
        self.cursors.get(name)
    }

    /// Mutable cursor access for `FETCH`.
    pub fn cursor_mut(&mut self, name: &str) -> Option<&mut SessionCursor> {
        self.cursors.get_mut(name)
    }

    /// Closes one cursor, or every cursor for `CLOSE ALL`.
    pub fn close_cursor(&mut self, name: Option<&str>) -> bool {
        match name {
            None => {
                let had_any = !self.cursors.is_empty();
                self.cursors.clear();
                had_any
            }
            Some(name) => self.cursors.remove(name).is_some(),
        }
    }

    /// Closes non-holdable cursors (`keep_hold`) or every cursor.
    fn drop_cursors(&mut self, keep_hold: bool) {
        if keep_hold {
            self.cursors.retain(|_, cursor| cursor.with_hold);
        }
        else {
            self.cursors.clear();
        }
    }

    /// Subscribes this session to `channel`.
    pub fn listen(&mut self, runtime: &PgsqlSqlRuntime, channel: &str) {
        let hub = Arc::clone(&runtime.notifications);
        let sink = self.sink(&hub);
        hub.listen(sink, channel);
    }

    /// Removes one subscription, or every subscription when `channel` is `None`.
    pub fn unlisten(&mut self, channel: Option<&str>) {
        let (Some(hub), Some(sink)) = (&self.notifications, self.listener)
        else {
            return;
        };
        match channel {
            Some(channel) => hub.unlisten(sink, channel),
            None => hub.unlisten_all(sink),
        }
    }

    /// Queues one notification for every subscriber of `channel`.
    pub fn notify(&self, runtime: &PgsqlSqlRuntime, channel: &str, payload: &str, process_id: u32) {
        runtime.notifications.notify(super::notify::Notification {
            process_id,
            channel: channel.to_owned(),
            payload: payload.to_owned(),
        });
    }

    /// Drains the notifications queued for this session.
    pub fn take_notifications(&mut self) -> Vec<super::notify::Notification> {
        let (Some(hub), Some(sink)) = (&self.notifications, self.listener)
        else {
            return Vec::new();
        };
        hub.take(sink)
    }

    /// Lazily registers the session sink on `hub`.
    fn sink(&mut self, hub: &Arc<super::notify::NotificationHub>) -> u64 {
        if let (Some(existing), Some(sink)) = (&self.notifications, self.listener) {
            if Arc::ptr_eq(existing, hub) {
                return sink;
            }
            existing.unregister(sink);
        }
        let sink = hub.register();
        self.notifications = Some(Arc::clone(hub));
        self.listener = Some(sink);
        sink
    }
}

impl Drop for TransactionSession {
    fn drop(&mut self) {
        if let (Some(hub), Some(sink)) = (&self.notifications, self.listener) {
            hub.unregister(sink);
        }
    }
}

impl Default for TransactionSession {
    fn default() -> Self {
        Self::new()
    }
}
