//! PostgreSQL session state: transaction blocks, the schema checkpoint and the
//! emulated GUCs the wire session reports.
//!
//! The session is execution state, not protocol state: the local engine and the
//! wire gateway both open a [`TransactionSession`] and run every statement
//! through [`crate::pgsql::dispatch_statement`], so block entry, schema
//! checkpointing and aborted-block rejection exist once.

use std::{
    io,
    sync::Arc,
};

use crate::cluster::{KeyValueExecutor, TransactionHandle};
use database_query::{SqlSchemaCheckpoint, SqlSchemaRegistry};

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
        }
    }

    /// Captures the registry state when a transaction block opens.
    fn capture_schema(&mut self, runtime: Option<&Arc<PgsqlSqlRuntime>>) {
        self.schema_checkpoint = runtime.map(|rt| rt.schema.checkpoint());
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
            // `COMMIT` of an aborted block is a rollback: the catalog goes with the rows.
            self.restore_schema(runtime)?;
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
        // The commit point publishes the schema revision as well.
        self.schema_checkpoint = None;
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
        self.restore_schema(runtime)?;
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
        // `DISCARD ALL` is rejected inside a transaction block, so there is no
        // pending checkpoint here; dropping the slot keeps the invariant that
        // only an open block owns schema rollback.
        self.schema_checkpoint = None;
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
}

impl Default for TransactionSession {
    fn default() -> Self {
        Self::new()
    }
}
