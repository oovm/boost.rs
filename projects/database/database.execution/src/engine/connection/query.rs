//! Query and DML execution on the connection record map.
//!
//! Two frontends reach the same executor: VOS (`prepare_vos`, `query_vos`,
//! `execute_vos_write`) and SQL (`prepare_sql`, `query_sql`, `execute_sql_write`).

use std::{collections::BTreeMap, sync::Arc};

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use crate::engine::{
    Error, Result,
    bytes::{ObjectStoreResolver, Placement},
};
use database_formats::yy::row::{ObjectResolver, with_object_resolver, with_row_schema};
use database_query::local::{Dialect, PreparedStatement, QueryKind, QueryRow, fingerprint as fp, prepare as prepare_source};
use database_types::Value;

use super::{Backend, Connection};

/// Result of [`Connection::execute_plan`], dispatched by the plan's kind.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOutcome {
    /// A read pipeline emitted these rows.
    Rows(Vec<QueryRow>),
    /// A write program changed this many rows.
    Affected(usize),
}

/// Cap for the per-connection statement caches. A workload that mints a fresh
/// source per statement must not grow them without bound, so an overflow drops
/// the whole cache instead of evicting one entry at a time.
const STATEMENT_CACHE_LIMIT: usize = 256;

/// 单条语句的阶段计时起点。
///
/// 原生目标用单调时钟 `Instant`。浏览器 WASM 目标上 `Instant::now()` 会
/// panic（`wasm-pack --target web` 关掉了 std 时钟），这里显式使用墙钟
/// `Date.now()` 并把时钟能力写进 [`ClockQuality`]，不再借用 KV 租约时钟：
/// 租约的绝对时间与语句耗时是两种语义，复用会把系统时间跳变伪装成耗时。
#[cfg(not(target_arch = "wasm32"))]
type Tick = Instant;

/// 浏览器 WASM 下的计时起点：墙钟毫秒时间戳。
#[cfg(target_arch = "wasm32")]
type Tick = u64;

#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn tick() -> Tick {
    Instant::now()
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn tick() -> Tick {
    js_sys::Date::now() as u64
}

#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn tick_nanos(started: Tick) -> u128 {
    started.elapsed().as_nanos()
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn tick_nanos(started: Tick) -> u128 {
    // 墙钟只有毫秒精度，换算回纳秒让 `*_ns` 字段保持同一单位。
    (js_sys::Date::now() as u64).saturating_sub(started) as u128 * 1_000_000
}

impl Connection {
    fn object_resolver(&self) -> Arc<dyn ObjectResolver> {
        Arc::new(ObjectStoreResolver::new(Arc::clone(&self.objects)))
    }

    /// The placement a write of this connection executes with: large `bytes`
    /// and `vector<N>` payloads land in the connection object store and the
    /// row keeps the content-addressed reference.
    fn placement(&self) -> Placement<'_> {
        Placement::Commit(&self.objects)
    }

    /// The placement a write of this connection plans with: the same
    /// content-addressed row values as [`Self::placement`], derived purely
    /// from the payload without writing anything. A failed plan, a repeated
    /// plan or an `EXPLAIN` therefore leaves no object behind.
    fn planning_placement(&self) -> Placement<'static> {
        Placement::Preview
    }

    /// Run `body` with the row resolver and the schema-derived named layouts in
    /// scope, so row decoding can resolve out-of-line references and a stored
    /// `unite` or `structure` value decodes under its declaration.
    ///
    /// Placement is no longer a callback: the write entries take an explicit
    /// [`Placement`] so planning stays pure and only execution writes.
    fn with_row_context<T>(&self, contract: &schema_types::ResolvedContract, body: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut directory = crate::engine::catalog::row_schema_directory(contract)?;
        {
            let history = self.layout_history.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for historical in history.values() {
                directory = directory.with_retained(historical);
            }
        }
        let directory = Arc::new(directory);
        with_row_schema(Some(directory), || with_object_resolver(Some(self.object_resolver()), body))
    }

    /// Prepare a VOS read pipeline into a reusable query plan.
    ///
    /// The returned [`PreparedStatement`] carries the lowered op tree, named
    /// parameter descriptors, source fingerprint and — when a schema is
    /// already bound on this connection — the current schema's fingerprint
    /// and revision. Executions later use those two fields to detect schema
    /// migration without re-parsing.
    pub fn prepare_vos(&self, source: &str) -> Result<PreparedStatement> {
        self.prepare_with(source, prepare_source, Dialect::Vos)
    }

    /// Prepare a SQL statement into a reusable query plan.
    ///
    /// The plan is the same operator set VOS lowers into; only the front half
    /// differs. `?` placeholders are declared as `?1`, `?2`, … parameters and
    /// bind positionally through [`Self::query_sql_with_params`] and
    /// [`Self::execute_sql_write`].
    ///
    /// **Not part of the `@yydb/yydb` public author surface.** SQL remains an
    /// internal execution path; downstream authors should use the VOS frontend
    /// only.
    #[doc(hidden)]
    pub fn prepare_sql(&self, source: &str) -> Result<PreparedStatement> {
        self.prepare_with(source, database_query::local::prepare_sql, Dialect::Sql)
    }

    fn prepare_with(&self, source: &str, frontend: fn(&str) -> Result<PreparedStatement>, dialect: Dialect) -> Result<PreparedStatement> {
        self.guard_session_alive()?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only the schema basis is needed here. An open transaction can read it
        // straight from its snapshot instead of cloning the whole state per
        // statement, which is what kept a batch load quadratic.
        let basis = {
            let slot = self.txn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.as_ref().map(schema_basis)
        };
        let basis = match basis {
            Some(basis) => basis,
            None => schema_basis(&self.read_state_metadata()?),
        };
        // The same source under the same schema basis lowers to the same plan,
        // so a repeated statement pays parse and lowering only once. A schema
        // change moves the basis and misses the cache.
        let key = (dialect, source.to_owned());
        let cached = self
            .prepared_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .filter(|(fingerprint, revision, _)| *fingerprint == basis.0 && *revision == basis.1)
            .map(|(_, _, stmt)| stmt.clone());
        if let Some(stmt) = cached {
            return Ok(stmt);
        }
        // The frontend already classifies ops and fills `query_kind` — the
        // engine path only adds schema fingerprint / revision on top.
        let mut stmt = frontend(source)?;
        stmt.schema_fingerprint = basis.0.clone();
        stmt.schema_revision = basis.1;
        let mut cache = self.prepared_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= STATEMENT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, (basis.0, basis.1, stmt.clone()));
        Ok(stmt)
    }

    /// Execute a previously prepared statement with named parameter bindings.
    ///
    /// Reads the active connection state, including an open transaction
    /// snapshot. Returns [`Error::QueryBind`] when `named` is missing required
    /// parameters, contains extraneous ones, or provides values that do not
    /// match the declared slot types.
    ///
    /// Returns [`Error::SchemaInvalidated`] when the active schema document
    /// has drifted from the fingerprint captured at prepare time. Re-prepare
    /// after schema migration.
    pub fn query_prepared(&self, stmt: &PreparedStatement, named: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        let values = database_query::local::bind_named(stmt, named)?;
        self.run_prepared_read(stmt, &values)
    }

    /// Execute a previously prepared statement with positional bindings.
    ///
    /// Values are matched to parameters in declaration order, which is how SQL
    /// `?` placeholders bind.
    pub fn query_prepared_ordered(&self, stmt: &PreparedStatement, values: &[Value]) -> Result<Vec<QueryRow>> {
        let values = database_query::local::bind_ordered(stmt, values)?;
        self.run_prepared_read(stmt, &values)
    }

    /// Same execution as [`Self::query_prepared_ordered`], with per-stage
    /// timings written into `profile`.
    ///
    /// Diagnostics only: the engine itself never reads these numbers. Timers
    /// are taken only on this path, so ordinary reads pay nothing for it.
    pub fn query_prepared_ordered_profiled(
        &self,
        stmt: &PreparedStatement,
        values: &[Value],
        profile: &mut ReadProfile,
    ) -> Result<Vec<QueryRow>> {
        let started = tick();
        let values = database_query::local::bind_ordered(stmt, values)?;
        profile.bind_ns += tick_nanos(started);
        self.run_prepared_read_profiled(stmt, &values, Some(profile))
    }

    fn run_prepared_read(&self, stmt: &PreparedStatement, values: &[Value]) -> Result<Vec<QueryRow>> {
        self.run_prepared_read_profiled(stmt, values, None)
    }

    fn run_prepared_read_profiled(
        &self,
        stmt: &PreparedStatement,
        values: &[Value],
        mut profile: Option<&mut ReadProfile>,
    ) -> Result<Vec<QueryRow>> {
        self.guard_session_alive()?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // A transaction already owns one full snapshot, so narrowing here
        // would read less than the snapshot the caller expects.
        if self.has_open_transaction() {
            let state = self.active_state_unlocked()?;
            let started = tick();
            self.check_schema_drift(stmt, &state)?;
            if let Some(profile) = profile.as_deref_mut() {
                profile.drift_ns += tick_nanos(started);
            }
            return self.execute_with_profiled(stmt, values, resolved_contract(&state)?, &state.records, profile);
        }

        // Phase one: metadata only. The decoded contract is what lets the
        // query layer prove which record-key window one statement can reach.
        let started = tick();
        let metadata = self.read_state_metadata()?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.metadata_ns += tick_nanos(started);
        }
        let started = tick();
        self.check_schema_drift(stmt, &metadata)?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.drift_ns += tick_nanos(started);
        }
        let started = tick();
        let window = database_query::local::read_window(&stmt.ops, resolved_contract(&metadata)?, values)?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.plan_ns += tick_nanos(started);
        }
        let Some(window) = window
        else {
            let state = self.read_state()?;
            self.check_schema_drift(stmt, &state)?;
            // The narrowing plan does not apply, but the profiled run must still
            // report the operator work instead of silently dropping it.
            return self.execute_with_profiled(stmt, values, resolved_contract(&state)?, &state.records, profile);
        };

        // Memory-backed connections already hold every row.
        if matches!(self.backend.as_ref(), Backend::Memory { .. }) {
            return self.execute_with_profiled(stmt, values, resolved_contract(&metadata)?, &metadata.records, profile);
        }

        // The window was derived from this metadata, so its schema basis must
        // still hold after the second read. Capture it before the metadata is
        // consumed by the window merge.
        let basis = schema_basis(&metadata);
        // Phase two: metadata plus the window, read as one pager transaction.
        // Table rows stay in the decoded page images; only metadata and, for
        // single-file images, the embedded object catalog are owned.
        let started = tick();
        let (state, rows) = self.read_state_window_rows(&window, metadata)?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.window_read_ns += tick_nanos(started);
        }
        if schema_basis(&state) != basis {
            // The durable schema moved between the two reads, so the window
            // was derived from a contract that no longer describes the data.
            // Fall back to a single full snapshot rather than trust it.
            let state = self.read_state()?;
            self.check_schema_drift(stmt, &state)?;
            return self.execute_with_profiled(stmt, values, resolved_contract(&state)?, &state.records, profile);
        }
        self.execute_with_rows(stmt, values, resolved_contract(&state)?, &state.records, rows.as_ref(), profile)
    }

    fn execute_with(
        &self,
        stmt: &PreparedStatement,
        values: &[Value],
        contract: &schema_types::ResolvedContract,
        records: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Vec<QueryRow>> {
        self.with_row_context(contract, || crate::local::execute_prepared(stmt, values, Some(contract), records))
    }

    fn execute_with_profiled(
        &self,
        stmt: &PreparedStatement,
        values: &[Value],
        contract: &schema_types::ResolvedContract,
        records: &BTreeMap<String, Vec<u8>>,
        profile: Option<&mut ReadProfile>,
    ) -> Result<Vec<QueryRow>> {
        let Some(profile) = profile
        else {
            return self.execute_with(stmt, values, contract, records);
        };
        let started = tick();
        let (rows, exec) = self.with_row_context(contract, || {
            crate::local::execute_prepared_profiled(stmt, values, Some(contract), records)
        })?;
        self.add_exec_profile(profile, started, &exec, rows.len());
        Ok(rows)
    }

    /// Execute a narrowed read whose table rows are borrowed from a window.
    ///
    /// `records` still carries the metadata rows the engine owns; table scans
    /// decode rows from `rows` and hand back owned `QueryRow`s, so no borrow
    /// leaves this call.
    fn execute_with_rows(
        &self,
        stmt: &PreparedStatement,
        values: &[Value],
        contract: &schema_types::ResolvedContract,
        records: &BTreeMap<String, Vec<u8>>,
        rows: Option<&database_storage::local::RowWindow>,
        profile: Option<&mut ReadProfile>,
    ) -> Result<Vec<QueryRow>> {
        let Some(rows) = rows
        else {
            // Memory-backed reads already own every row, so they run unwindowed.
            return self.execute_with_profiled(stmt, values, contract, records, profile);
        };
        let Some(profile) = profile
        else {
            let (out, _) = self.with_row_context(contract, || {
                crate::local::execute_prepared_borrowed(stmt, values, Some(contract), records, rows, false)
            })?;
            return Ok(out);
        };
        let started = tick();
        let (out, exec) = self.with_row_context(contract, || {
            crate::local::execute_prepared_borrowed(stmt, values, Some(contract), records, rows, true)
        })?;
        self.add_exec_profile(profile, started, &exec, out.len());
        Ok(out)
    }

    fn add_exec_profile(&self, profile: &mut ReadProfile, started: Tick, exec: &crate::local::ExecProfile, emitted_rows: usize) {
        profile.exec_ns += tick_nanos(started);
        profile.scanned_rows += exec.scanned_rows;
        profile.decoded_rows += exec.decoded_rows;
        profile.emitted_rows += emitted_rows as u64;
        profile.index_probes += exec.index_probes;
        profile.fused_ops += exec.fused_ops;
        profile.scan_ns += exec.scan_ns;
        profile.filter_ns += exec.filter_ns;
        profile.project_ns += exec.project_ns;
        profile.sort_ns += exec.sort_ns;
        profile.exec_other_ns += exec.other_ns;
    }

    fn check_schema_drift(&self, stmt: &PreparedStatement, state: &super::State) -> Result<()> {
        // Schema-fingerprint gate: only when the statement was prepared with
        // a live schema context (non-empty fingerprint).
        if stmt.schema_fingerprint.is_empty() {
            return Ok(());
        }
        let Some(schema) = &state.schema
        else {
            return Err(Error::SchemaInvalidated {
                expected_fingerprint: stmt.schema_fingerprint.clone(),
                current_fingerprint: String::new(),
                schema_revision: stmt.schema_revision,
            });
        };
        let current_fp = fp(&schema.document);
        if current_fp != stmt.schema_fingerprint {
            return Err(Error::SchemaInvalidated {
                expected_fingerprint: stmt.schema_fingerprint.clone(),
                current_fingerprint: current_fp,
                schema_revision: stmt.schema_revision,
            });
        }
        Ok(())
    }

    /// Convenience wrapper: prepare → bind → execute in one call.
    ///
    /// Equivalent to [`Self::prepare_vos`] followed by [`Self::query_prepared`]
    /// on the same connection. Prefer this when you only intend to run the
    /// source once with a single parameter set; use the explicit
    /// prepare-then-bind path when you need to re-run the same source.
    pub fn query_vos_with_params(&self, source: &str, named: &BTreeMap<String, Value>) -> Result<Vec<QueryRow>> {
        let stmt = self.prepare_vos(source)?;
        self.query_prepared(&stmt, named)
    }

    /// Execute a VOS read pipeline with no bound parameters.
    ///
    /// Internally calls [`Self::prepare_vos`] followed by
    /// [`Self::query_prepared`] with an empty parameter map.
    ///
    /// Reads the active connection state, including an open transaction
    /// snapshot. Requires [`Self::ensure_schema`] first.
    pub fn query_vos(&self, source: &str) -> Result<Vec<QueryRow>> {
        self.query_vos_with_params(source, &BTreeMap::new())
    }

    /// Execute a SQL read statement with positional parameter values.
    ///
    /// **Not part of the `@yydb/yydb` public author surface.**
    #[doc(hidden)]
    pub fn query_sql_with_params(&self, source: &str, values: &[Value]) -> Result<Vec<QueryRow>> {
        let stmt = self.prepare_sql(source)?;
        self.query_prepared_ordered(&stmt, values)
    }

    /// Execute a SQL read statement with no parameters.
    ///
    /// **Not part of the `@yydb/yydb` public author surface.**
    #[doc(hidden)]
    pub fn query_sql(&self, source: &str) -> Result<Vec<QueryRow>> {
        self.query_sql_with_params(source, &[])
    }

    /// Exact record windows for the keys a VOS point write names.
    fn key_windows(keys: Vec<String>) -> Vec<database_query::local::ReadWindow> {
        keys.into_iter().map(|key| database_query::local::ReadWindow { end: Some(format!("{key}\u{0}")), start: key }).collect()
    }

    /// Whether every rule-bearing table already has a warm unique-key index.
    ///
    /// This is what lets a VOS write on a rule table narrow to its exact keys
    /// outside a transaction: the duplicate check reads the cache instead of
    /// the table, so the statement no longer needs every row loaded.
    fn rules_warm(
        &self,
        contract: &schema_types::ResolvedContract,
        declared: &[database_types::TableUniqueConstraint],
        basis: &(String, u64),
    ) -> bool {
        let Ok(document) = database_query::local::constraints::from_contract(contract)
        else {
            return false;
        };
        let merged = database_query::local::constraints::merge(document, declared);
        let mut seen: Vec<&str> = Vec::new();
        for rule in &merged {
            let table = rule.table();
            if seen.contains(&table) {
                continue;
            }
            seen.push(table);
            if !self.unique_index.is_warm(table, (basis.0.as_str(), basis.1)) {
                return false;
            }
        }
        true
    }

    /// Execute a VOS write program (inserts and schema macros).
    ///
    /// Mutates the active connection state. When a transaction is open, changes
    /// stay in the snapshot until [`Self::commit`] or [`Self::rollback`].
    pub fn execute_vos_write(&self, source: &str) -> Result<()> {
        self.execute_vos_write_with_params(source, &BTreeMap::new())
    }

    /// Execute a VOS insert program with named `$name` parameter bindings.
    ///
    /// Parameters reach a field initializer as host values, so a `bytes`
    /// parameter moves the octets straight into the column: values above the
    /// storage's inline boundary become a content-addressed object reference in
    /// the row, at or below it stay inline. Mutations follow the same
    /// transaction rules as [`Self::execute_vos_write`].
    pub fn execute_vos_write_with_params(&self, source: &str, named: &BTreeMap<String, Value>) -> Result<()> {
        self.execute_vos_write_params_counted(source, named).map(|_| ())
    }

    /// Same write as [`Self::execute_vos_write_with_params`], returning how many
    /// write statements actually ran.
    ///
    /// The prepared-write entry point needs that count for its affected result,
    /// while the plain surface keeps its `Result<()>` signature.
    fn execute_vos_write_params_counted(&self, source: &str, named: &BTreeMap<String, Value>) -> Result<usize> {
        self.guard_session_alive()?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let affected = std::cell::Cell::new(0usize);
        self.mutate_narrowed_write_unlocked(
            |metadata, holds_every_row| {
                let contract = resolved_contract(metadata)?;
                let schema_document = metadata.schema.as_ref().map(|schema| schema.document.as_str());
                let basis = schema_basis(metadata);
                // Scan, parse and macro expansion depend only on the source and
                // the schema document, so they are cached; each statement re-binds
                // its parameter values.
                let parsed = self
                    .bound_write_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(source)
                    .filter(|(fingerprint, revision, _)| *fingerprint == basis.0 && *revision == basis.1)
                    .map(|(_, _, parsed)| parsed.clone());
                let plan = match parsed {
                    Some(parsed) => parsed.bind(named)?,
                    None => {
                        let parsed = database_query::local::parse_vos_write(source, schema_document)?;
                        let mut cache = self.bound_write_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if cache.len() >= STATEMENT_CACHE_LIMIT {
                            cache.clear();
                        }
                        cache.insert(source.to_owned(), (basis.0.clone(), basis.1, parsed.clone()));
                        parsed.bind(named)?
                    }
                };
                let exempt = holds_every_row || self.rules_warm(contract, &metadata.unique_constraints, &basis);
                let keys = self
                    .with_row_context(contract, || {
                        let mut place = |value| self.planning_placement().place(value);
                        database_query::local::point_write_keys(
                            &plan.program,
                            contract,
                            &metadata.unique_constraints,
                            schema_document,
                            &plan.values,
                            exempt,
                            &mut place,
                        )
                    })?
                    .map(Self::key_windows);
                let complete = keys.is_none() || holds_every_row;
                Ok((keys, (plan, basis, complete)))
            },
            |state, (plan, basis, complete)| {
                // The write executor mutates `state.records`, so it cannot hold a
                // borrow of the contract inside the same state.
                let contract = resolved_contract(state)?.clone();
                let schema_document = state.schema.as_ref().map(|schema| schema.document.as_str());
                let records = &mut state.records;
                let constraints = &state.unique_constraints;
                let values = &plan.values;
                let index = database_query::local::constraints::WriteIndex { index: &self.unique_index, basis, complete };
                let placement = self.placement();
                let merged = database_query::local::constraints::merge(
                    database_query::local::constraints::from_contract(&contract)?,
                    constraints,
                );
                let count = self.with_row_context(&contract, || {
                    let mut place = |value| placement.place(value);
                    let ops = database_query::local::lower_write_program(
                        &plan.program,
                        &contract,
                        schema_document,
                        values,
                        &mut place,
                    )?;
                    let outcome = crate::local::execute_ops(&ops, &contract, &merged, records, values, &placement, Some(&index))?;
                    Ok::<usize, database_types::Error>(outcome.affected)
                })?;
                affected.set(count);
                Ok(())
            },
        )?;
        Ok(affected.get())
    }

    /// Execute a SQL write statement (INSERT / UPDATE / DELETE) with positional
    /// parameter values. Returns the number of affected rows.
    ///
    /// Mutates the active connection state. When a transaction is open, changes
    /// stay in the snapshot until [`Self::commit`] or [`Self::rollback`].
    ///
    /// **Not part of the `@yydb/yydb` public author surface.**
    #[doc(hidden)]
    pub fn execute_sql_write(&self, source: &str, values: &[Value]) -> Result<usize> {
        let stmt = self.prepare_sql(source)?;
        self.execute_prepared_write(&stmt, values)
    }

    /// Shared write tail of [`Self::execute_sql_write`] and [`Self::execute_plan`]:
    /// everything after prepare, with `values` bound positionally against the
    /// plan's declared parameters.
    ///
    /// A VOS write program carries no lowered `write_ops` — `prepare` classifies
    /// it as a write and leaves the operator tree empty — so it cannot run
    /// through the SQL write executor. Those plans are routed back through the
    /// VOS frontend by source text instead of silently reporting zero rows.
    fn execute_prepared_write(&self, stmt: &PreparedStatement, values: &[Value]) -> Result<usize> {
        let values = database_query::local::bind_ordered(stmt, values)?;
        if stmt.dialect == Dialect::Vos && stmt.write_ops.is_empty() && stmt.query_kind == QueryKind::Write {
            // The VOS executor binds by name, so rebuild the named map from the
            // declaration list and the positional slots `bind_ordered` just
            // validated. Its own entry point takes the operation lock, which
            // this function has not taken yet.
            let named = stmt
                .parameters
                .iter()
                .map(|parameter| (parameter.name.clone(), values[parameter.index as usize].clone()))
                .collect::<BTreeMap<String, Value>>();
            return self.execute_vos_write_params_counted(&stmt.source, &named);
        }
        self.guard_session_alive()?;
        let _guard = self.operation_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut affected = 0;
        self.mutate_narrowed_write_unlocked(
            |metadata, holds_every_row| {
                let contract = resolved_contract(metadata)?;
                let basis = schema_basis(metadata);
                // The same warm-index exemption the VOS write path uses: when the
                // shared unique-key cache already covers every rule table, the
                // duplicate check reads the cache instead of the whole table.
                let exempt = holds_every_row || self.rules_warm(contract, &metadata.unique_constraints, &basis);
                // Window planning is pure: `write_windows` only reads the bound
                // values, so no placement is involved and nothing is written.
                let windows = database_query::local::write_windows(
                    &stmt.write_ops,
                    &contract,
                    &metadata.unique_constraints,
                    &values,
                    holds_every_row,
                    exempt,
                )?;
                // `write_windows` widens a cold-rule write with the whole table of
                // every rule target, so the narrowed snapshot still answers the
                // from-records rebuild a cold index needs: that is complete for
                // [`constraints::WriteIndex`]. A warm exempt write keeps only point
                // keys and stays partial, which is exactly what the warm index
                // answers from.
                let complete = windows.is_none() || holds_every_row || !exempt;
                Ok((windows, (basis, complete)))
            },
            |state, (basis, complete)| {
                self.check_schema_drift(&stmt, state)?;
                // The write executor mutates `state.records`, so it cannot hold a
                // borrow of the contract inside the same state.
                let contract = resolved_contract(state)?.clone();
                // The same rule set the VOS frontend enforces: the attributes the
                // schema contract declares plus the host-declared rules. Without
                // the merge an SQL write would skip both the duplicate check and
                // the index maintenance for document-derived constraints.
                let constraints = database_query::local::constraints::merge(
                    database_query::local::constraints::from_contract(&contract)?,
                    &state.unique_constraints,
                );
                let index = database_query::local::constraints::WriteIndex { index: &self.unique_index, basis, complete };
                let placement = self.placement();
                let outcome = self.with_row_context(&contract, || {
                    crate::local::execute_write_prepared(
                        &stmt,
                        &values,
                        &contract,
                        &constraints,
                        &mut state.records,
                        &placement,
                        Some(&index),
                    )
                })?;
                affected = outcome.affected;
                Ok(())
            },
        )?;
        Ok(affected)
    }

    /// Execute a caller-supplied plan directly, bypassing both text frontends.
    ///
    /// The plan may be a cached [`Self::prepare_vos`] / [`Self::prepare_sql`]
    /// result or a hand-built [`PreparedStatement`]; `values` bind
    /// positionally against the plan's declared parameters (empty when the
    /// plan embeds its values). Reads run the same narrowed-window path as
    /// prepared queries, writes the same transaction-narrowing path as SQL
    /// DML, and both still check schema drift against the fingerprint and
    /// revision captured on the plan.
    pub fn execute_plan(&self, stmt: &PreparedStatement, values: &[Value]) -> Result<PlanOutcome> {
        match stmt.query_kind {
            QueryKind::Read => Ok(PlanOutcome::Rows(self.query_prepared_ordered(stmt, values)?)),
            QueryKind::Write => Ok(PlanOutcome::Affected(self.execute_prepared_write(stmt, values)?)),
            QueryKind::Session => Err(Error::Unsupported("execute_plan does not run session statements")),
        }
    }
}

/// Per-stage timings and work counts of one prepared read.
///
/// Filled only by [`Connection::query_prepared_ordered_profiled`]; ordinary
/// reads never take timers. Values accumulate across calls, so a caller that
/// runs the same statement many times can divide by the call count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadProfile {
    /// Parameter binding and slot validation.
    pub bind_ns: u128,
    /// Reading reserved metadata and decoding the stored schema and contract.
    pub metadata_ns: u128,
    /// Schema drift check, including the schema document fingerprint.
    pub drift_ns: u128,
    /// Window planning: which record keys one statement can reach.
    pub plan_ns: u128,
    /// Pager read of the planned window plus its row decode into the record map.
    pub window_read_ns: u128,
    /// Whole-row plan execution: table decode, filters, projection, sorting.
    pub exec_ns: u128,
    /// Table scan inside [`Self::exec_ns`]: record-map walk and row decode.
    pub scan_ns: u128,
    /// Row filtering inside [`Self::exec_ns`].
    pub filter_ns: u128,
    /// Projection rebuild inside [`Self::exec_ns`].
    pub project_ns: u128,
    /// Ordering inside [`Self::exec_ns`].
    pub sort_ns: u128,
    /// Joins, set operations, aggregates and windows inside [`Self::exec_ns`].
    pub exec_other_ns: u128,
    /// Rows storage handed to table scans, summed across calls.
    pub scanned_rows: u64,
    /// Row records decoded from storage, summed across calls.
    pub decoded_rows: u64,
    /// Rows emitted to the caller, summed across calls.
    pub emitted_rows: u64,
    /// Primary-key probes that replaced a scan plus filter, summed across calls.
    pub index_probes: u64,
    /// Operators fused away by the primary-key shortcut, summed across calls.
    pub fused_ops: u64,
    /// Clock every `*_ns` field was measured with.
    pub clock: ClockQuality,
}

impl ReadProfile {
    /// Sum of every recorded stage.
    pub fn total_ns(&self) -> u128 {
        self.bind_ns + self.metadata_ns + self.drift_ns + self.plan_ns + self.window_read_ns + self.exec_ns
    }
}

/// Clock a profile's `*_ns` fields were measured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockQuality {
    /// Clock source the timings come from.
    pub source: ClockSource,
    /// Nominal resolution in nanoseconds.
    pub resolution_ns: u64,
}

impl ClockQuality {
    /// Capability of the clock this build takes timings with.
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self { source: ClockSource::Monotonic, resolution_ns: 1 }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self { source: ClockSource::Wall, resolution_ns: 1_000_000 }
        }
    }
}

impl Default for ClockQuality {
    fn default() -> Self {
        Self::current()
    }
}

/// Where a profile's timings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// Monotonic clock (`std::time::Instant`): elapsed time never jumps.
    Monotonic,
    /// Wall clock (`Date.now()` in the browser): may jump with system time.
    Wall,
    /// No clock on this platform: every `*_ns` field stays zero.
    Unavailable,
}

fn resolved_contract(state: &super::State) -> Result<&schema_types::ResolvedContract> {
    state.resolved_contract.as_deref().ok_or_else(|| Error::Schema { message: "query requires ensure_schema before execution".into() })
}

/// Durable schema identity a read window was derived from: the schema
/// document fingerprint and the identity-manifest revision.
fn schema_basis(state: &super::State) -> (String, u64) {
    let fingerprint = state.schema.as_ref().map(|schema| fp(&schema.document)).unwrap_or_default();
    let revision = state.identity_history.as_ref().map(|history| history.revision).unwrap_or(0);
    (fingerprint, revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUT-01: the SQL write path maintains the shared unique-key cache
    /// instead of dropping it, both frontends keep rejecting duplicate keys,
    /// and a rolled-back insert leaves rows and cache consistent.
    #[test]
    fn sql_writes_keep_the_unique_index_warm_and_rollback_stays_consistent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.ensure_schema("table Member { @@id: i64, [unique] email: utf8 }").unwrap();
        // Warm the index through the VOS frontend.
        conn.execute_vos_write(r#"Member.insert({ id: 1, email: "a@example.com" })"#).unwrap();
        let state = conn.read_state().unwrap();
        let basis = schema_basis(&state);
        drop(state);
        let index = &conn.unique_index;
        let basis_ref = (basis.0.as_str(), basis.1);
        assert!(index.is_warm("Member", basis_ref), "the VOS write must have warmed the index");
        // The SQL frontend must maintain the warm index incrementally rather
        // than invalidate it wholesale.
        conn.execute_sql_write("INSERT INTO Member (id, email) VALUES (2, 'b@example.com')", &[]).unwrap();
        assert!(index.is_warm("Member", basis_ref), "an SQL write must not invalidate the warm unique index");
        assert_eq!(index.len("Member", basis_ref), Some(2), "both keys must sit in the same warm index");
        // Both frontends still reject a duplicate key.
        let duplicate_vos = conn.execute_vos_write(r#"Member.insert({ id: 3, email: "a@example.com" })"#);
        assert!(matches!(duplicate_vos, Err(Error::UniqueViolation { .. })), "{duplicate_vos:?}");
        let duplicate_sql = conn.execute_sql_write("INSERT INTO Member (id, email) VALUES (4, 'b@example.com')", &[]);
        assert!(matches!(duplicate_sql, Err(Error::UniqueViolation { .. })), "{duplicate_sql:?}");
        assert_eq!(conn.query_sql("SELECT id FROM Member").unwrap().len(), 2, "rejected inserts stored nothing");
        // A rolled-back insert must not leave a phantom key in the cache.
        conn.begin().unwrap();
        conn.execute_vos_write(r#"Member.insert({ id: 5, email: "c@example.com" })"#).unwrap();
        conn.rollback().unwrap();
        assert_eq!(conn.query_sql("SELECT id FROM Member").unwrap().len(), 2);
        conn.execute_vos_write(r#"Member.insert({ id: 5, email: "c@example.com" })"#).unwrap();
        assert_eq!(conn.query_sql("SELECT id FROM Member").unwrap().len(), 3, "the rolled-back key must not be cached");
    }
}
