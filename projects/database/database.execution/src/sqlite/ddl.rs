/*!
SQLite DDL / schema 持久化机器。

本模块负责与具体语句信封解耦的模式演进机制：

- `load_catalog`：把 `sqlite_master` b-tree 同步进 [`SqliteCatalog`]；
- `schema_objects` / `reindex_tables` / `reindex_temp_tables`：模式读回与目录位置重建；
- `run_alter_table`：`ALTER TABLE` 的改名、改列、加列、删列分支，同步改写 `sqlite_master` 与目录；
- `create_table_as` 与 `ctas_*`：`CREATE TABLE ... AS SELECT` 的列名 / 声明类型推导与建表灌数；
- `write_vacuum_image` / `open_attached_database`：`VACUUM INTO` 与 `ATTACH` 的纯文件级写出、打开机器。

语句信封（`ExecResult`）、`attached` 引擎句柄列表与事务/会话编排留在门面；本模块只接收显式借用，不依赖门面类型。
*/

use std::{collections::BTreeMap, path::PathBuf, rc::Rc};

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, storage_failure, value::*};
use database_storage::sqlite::{ColumnSpec, SchemaObject, SchemaObjectKind, SqliteWriter, TableLimits, WriteValue};

use super::{
    dml,
    eval::Budget,
    host::HostFunctionTable,
    select::{ExecContext, SqliteExecState, TableScope, statement_mentions_column, storage_error, unique_output_names},
};

/// `sqlite_master` and its aliases.
pub fn is_schema_table(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "sqlite_master" | "sqlite_schema" | "sqlite_temp_master" | "sqlite_temp_schema")
}

/// SQLite reserves every `sqlite_` name for internal objects.
pub fn check_reserved_object_name(name: &str, statement: &'static str) -> Result<(), SqlError> {
    if name.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(SqlError::new("reserved-name", format!("object name reserved for internal use: {name}")).in_statement(statement));
    }
    Ok(())
}

/// Replays the stored `CREATE TABLE` / `CREATE INDEX` text of every
/// schema row into the in-memory catalog without touching storage.
pub fn load_catalog(writer: &SqliteWriter, catalog: &mut SqliteCatalog) -> Result<(), SqlError> {
    let objects = writer.schema_objects(TableLimits::query()).map_err(storage_error)?;
    catalog.load_schema(&objects)
}

/// Reads the live schema rows with a statement-tagged storage error.
pub fn schema_objects(writer: &SqliteWriter, statement: &'static str) -> Result<Vec<SchemaObject>, SqlError> {
    writer.schema_objects(TableLimits::query()).map_err(|error| storage_failure(error).in_statement(statement))
}

/// Rebuilds the lowercase name to position map after a table is removed.
pub fn reindex_tables(catalog: &mut SqliteCatalog) {
    catalog.table_lookup.clear();
    for (position, table) in catalog.tables.iter().enumerate() {
        catalog.table_lookup.insert(table.name.to_ascii_lowercase(), position);
    }
}

/// Rebuilds the temp schema name map after a temp table is removed.
pub fn reindex_temp_tables(catalog: &mut SqliteCatalog) {
    catalog.temp_lookup.clear();
    for (position, table) in catalog.temp_tables.iter().enumerate() {
        catalog.temp_lookup.insert(table.def.name.to_ascii_lowercase(), position);
    }
}

/// SQLite's standard type name for one declared type: the affinity class the
/// type text resolves to, or an empty string for the BLOB affinity.
fn standard_affinity_name(declared: &str) -> String {
    let upper = declared.to_ascii_uppercase();
    let name = if upper.contains("INT") {
        "INT"
    }
    else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    }
    else if declared.is_empty() || upper.contains("BLOB") {
        ""
    }
    else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    }
    else {
        "NUM"
    };
    name.to_string()
}

/// Resolves a `column` reference of a CTAS output item to the unique source
/// column that defines it.
fn ctas_lookup_column<'a>(reference: &ColumnRef, sources: &'a [(String, Vec<(String, String)>)]) -> Option<&'a (String, String)> {
    let qualifier = reference.qualifier.as_deref();
    let mut found = None;
    for (label, columns) in sources {
        if let Some(qualifier) = qualifier {
            if !label.eq_ignore_ascii_case(qualifier) {
                continue;
            }
        }
        for column in columns {
            if column.0.eq_ignore_ascii_case(&reference.name) {
                if found.is_some() {
                    return None;
                }
                found = Some(column);
            }
        }
    }
    found
}

/// Output label of one CTAS item: the source column's own spelling for a bare
/// reference, otherwise the expression's label.
fn ctas_expression_label(expression: &Expr, sources: &[(String, Vec<(String, String)>)]) -> String {
    if let Expr::Column(reference) = expression {
        if let Some(column) = ctas_lookup_column(reference, sources) {
            return column.0.clone();
        }
    }
    expression.label()
}

/// Declared type of one CTAS output item: a bare column reference copies its
/// source affinity, every other expression gets no declared type.
fn ctas_expression_type(expression: &Expr, sources: &[(String, Vec<(String, String)>)]) -> String {
    match expression {
        Expr::Alias { value, .. } => ctas_expression_type(value, sources),
        Expr::Column(reference) => ctas_lookup_column(reference, sources).map(|column| standard_affinity_name(&column.1)).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Quotes a schema identifier when the generated `CREATE TABLE` text needs it.
fn ctas_quote_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_');
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// `sqlite_master.sql` text of a `CREATE TABLE ... AS SELECT` result, built the
/// same way SQLite generates it. SQLite switches to a multi-line layout for
/// long definitions; the compact form is emitted here because every reader
/// (including this parser) accepts it.
fn ctas_schema_sql(name: &str, columns: &[ColumnSpec]) -> String {
    let mut sql = format!("CREATE TABLE {}(", ctas_quote_name(name));
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str(&ctas_quote_name(&column.name));
        if !column.declared_type.is_empty() {
            sql.push(' ');
            sql.push_str(&column.declared_type);
        }
    }
    sql.push(')');
    sql
}

/// DDL / schema 持久化机器的执行上下文：由门面按显式借用组装，形态与 [`super::dml::DmlContext`] 一致。
pub struct DdlContext<'a> {
    /// 主库存储 writer。
    pub writer: &'a mut SqliteWriter,
    /// 内存目录。
    pub catalog: &'a mut SqliteCatalog,
    /// 会话执行状态。
    pub exec_state: &'a SqliteExecState,
    /// 宿主函数表。
    pub host_functions: &'a HostFunctionTable,
    /// 绑定参数。
    pub parameters: &'a [Cell],
    /// `UPSERT` 排除行（DDL 路径恒为空）。
    pub upsert_excluded: &'a [Cell],
    /// 触发器帧栈。
    pub trigger_frames: &'a [TriggerFrame],
    /// 当前引擎的 attached 别名。
    pub alias: Option<&'a str>,
    /// attached 引擎执行视图。
    pub attached: Rc<[ExecContext<'a>]>,
    /// 小写别名到 `attached` 位置的映射。
    pub attached_lookup: &'a BTreeMap<String, usize>,
    /// 当前名字解析作用域。
    pub scope: TableScope,
}

impl<'a> DdlContext<'a> {
    /// 组装 CTAS 子查询使用的只读执行视图。
    pub fn exec(&self) -> ExecContext<'_> {
        ExecContext {
            writer: self.writer,
            catalog: self.catalog,
            state: self.exec_state,
            host_functions: self.host_functions,
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            alias: self.alias,
            attached: Rc::clone(&self.attached),
            attached_lookup: self.attached_lookup,
        }
    }

    /// Executes `CREATE TABLE name AS SELECT ...`.
    ///
    /// The declared columns are derived from the query output: a bare column
    /// reference copies its source column's affinity as SQLite's standard
    /// type name (`INT`, `TEXT`, `REAL`, `NUM`), and every other expression
    /// gets no declared type. Duplicate output labels are numbered `:1`, `:2`.
    pub fn create_table_as(&mut self, name: String, if_not_exists: bool, temp: bool, query: &Statement) -> Result<(), SqlError> {
        let key = name.to_ascii_lowercase();
        check_reserved_object_name(&name, "create-table")?;
        let temp_target = temp || self.scope == TableScope::Temp;
        if temp_target {
            if self.catalog.temp_lookup.contains_key(&key) || self.catalog.temp_views.contains_key(&key) {
                if if_not_exists {
                    return Ok(());
                }
                return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
            }
        }
        else {
            if self.catalog.indexes.contains_key(&key) {
                return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-table"));
            }
            if self.catalog.views.contains_key(&key) {
                return Err(SqlError::new("view-exists", format!("there is already a view named {name}")).in_statement("create-table"));
            }
            if self.catalog.table_lookup.contains_key(&key) {
                if if_not_exists {
                    return Ok(());
                }
                return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
            }
        }
        let budget = Budget::new();
        let selected = self.exec().select_statement(query, &[], &budget, None)?;
        let labels = unique_output_names(&selected.columns);
        let mut types: Vec<String> =
            ctas_statement_columns(self.catalog, query, &BTreeMap::new())?.into_iter().map(|(_, declared)| declared).collect();
        types.resize(labels.len(), String::new());
        types.truncate(labels.len());
        let columns: Vec<ColumnSpec> = labels
            .iter()
            .zip(types)
            .map(|(name, declared_type)| ColumnSpec {
                name: name.clone(),
                declared_type,
                not_null: false,
                default_value: None,
                default_text: None,
                primary_key: false,
                unique: false,
                autoincrement: false,
                collate: None,
            })
            .collect();
        let sql = ctas_schema_sql(&name, &columns);
        let mut def = SqliteCatalog::table_entry(name.clone(), columns.clone(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), false, false)?;
        if temp_target {
            def.temp = true;
            let rootpage = match self.catalog.temp_free_pages.pop_first() {
                Some(page) => page,
                None => {
                    let page = self.catalog.temp_next_page;
                    self.catalog.temp_next_page += 1;
                    page
                }
            };
            let master_rowid = self.catalog.temp_next_rowid;
            self.catalog.temp_next_rowid += 1;
            let mut rows = BTreeMap::new();
            for row in &selected.rows {
                let slots: Vec<Option<WriteValue>> = row.iter().map(|cell| Some(dml::cell_write_value(cell))).collect();
                dml::temp_insert_row(&def, &mut rows, &slots, &[])?;
            }
            let position = self.catalog.temp_tables.len();
            self.catalog.temp_lookup.insert(key, position);
            self.catalog.temp_tables.push(TempTable { def, sql, rootpage, master_rowid, rows });
            self.catalog.temp_created = true;
            return Ok(());
        }
        self.writer
            .create_table_with_primary_key_sql(&name, &columns, &[], Some(&sql))
            .map_err(|error| storage_failure(error).in_statement("create-table"))?;
        if !selected.rows.is_empty() {
            self.writer.begin().map_err(|error| storage_failure(error).in_statement("create-table"))?;
            for row in &selected.rows {
                let slots: Vec<Option<WriteValue>> = row.iter().map(|cell| Some(dml::cell_write_value(cell))).collect();
                if let Err(error) = self.writer.insert(&name, &slots) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("create-table"));
                }
            }
            self.writer.commit().map_err(|error| storage_failure(error).in_statement("create-table"))?;
        }
        let position = self.catalog.tables.len();
        self.catalog.table_lookup.insert(key, position);
        self.catalog.tables.push(def);
        Ok(())
    }
}

/// Output column labels and copied declared types of one query, used to
/// build the schema of `CREATE TABLE ... AS SELECT`.
fn ctas_statement_columns(
    catalog: &SqliteCatalog,
    statement: &Statement,
    inherited: &BTreeMap<String, Vec<(String, String)>>,
) -> Result<Vec<(String, String)>, SqlError> {
    let Statement::Select { with, first, .. } = statement
    else {
        return Ok(Vec::new());
    };
    let mut ctes = inherited.clone();
    for entry in with {
        let mut columns = ctas_statement_columns(catalog, &entry.query, &ctes)?;
        if let Some(names) = &entry.columns {
            if names.len() == columns.len() {
                for (column, name) in columns.iter_mut().zip(names) {
                    column.0 = name.clone();
                }
            }
        }
        ctes.insert(entry.name.to_ascii_lowercase(), columns);
    }
    let mut sources: Vec<(String, Vec<(String, String)>)> = Vec::with_capacity(first.from.len());
    for table in &first.from {
        let label = table.alias.clone().unwrap_or_else(|| table.name.clone());
        sources.push((label, ctas_table_columns(catalog, table, &ctes)?));
    }
    let mut output = Vec::new();
    match &first.items {
        SelectItems::Named(expressions) => {
            for expression in expressions {
                let label = ctas_expression_label(expression, &sources);
                let declared = ctas_expression_type(expression, &sources);
                output.push((label, declared));
            }
        }
        SelectItems::CountAll => output.push(("count(*)".to_string(), String::new())),
        SelectItems::Star => {
            for (_, columns) in &sources {
                output.extend(columns.iter().cloned());
            }
        }
    }
    Ok(output)
}

/// Columns one `FROM` item exposes to the output of a CTAS query.
fn ctas_table_columns(
    catalog: &SqliteCatalog,
    table: &TableRef,
    ctes: &BTreeMap<String, Vec<(String, String)>>,
) -> Result<Vec<(String, String)>, SqlError> {
    if let Some(query) = &table.query {
        return ctas_statement_columns(catalog, query, ctes);
    }
    let name = table.name.to_ascii_lowercase();
    if let Some(columns) = ctes.get(&name) {
        return Ok(columns.clone());
    }
    if let Some(def) = catalog.tables.iter().find(|def| def.name.eq_ignore_ascii_case(&table.name)) {
        return Ok(def.columns.iter().map(|column| (column.name.clone(), standard_affinity_name(&column.declared_type))).collect());
    }
    if let Some(temp) = catalog.temp_tables.iter().find(|temp| temp.def.name.eq_ignore_ascii_case(&table.name)) {
        return Ok(temp.def.columns.iter().map(|column| (column.name.clone(), standard_affinity_name(&column.declared_type))).collect());
    }
    if let Some(view) = catalog.views.get(&name) {
        let mut columns = ctas_statement_columns(catalog, &view.query, ctes)?;
        if let Some(names) = &view.columns {
            if names.len() == columns.len() {
                for (column, name) in columns.iter_mut().zip(names) {
                    column.0 = name.clone();
                }
            }
        }
        return Ok(columns);
    }
    Ok(Vec::new())
}

/// Executes the `ALTER TABLE` forms: `RENAME TO`, `RENAME COLUMN`, `ADD
/// COLUMN` and `DROP COLUMN`. A drop rewrites every stored record and the
/// stored `CREATE TABLE` text, refusing columns still read by an index,
/// a view, a trigger or a table-level constraint.
pub fn run_alter_table(writer: &mut SqliteWriter, catalog: &mut SqliteCatalog, table: String, action: AlterAction) -> Result<(), SqlError> {
    if is_schema_table(&table) {
        return Err(SqlError::new("readonly-system-table", format!("table {table} may not be altered")).in_statement("alter-table"));
    }
    let objects = schema_objects(writer, "alter-table")?;
    if table.eq_ignore_ascii_case("sqlite_sequence") {
        let present = objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&table));
        if present {
            return Err(SqlError::new("readonly-system-table", format!("table {table} may not be altered")).in_statement("alter-table"));
        }
        return Err(SqlError::new("unknown-table", format!("no such table: {table}")).in_statement("alter-table"));
    }
    let key = table.to_ascii_lowercase();
    let position = *catalog
        .table_lookup
        .get(&key)
        .ok_or_else(|| SqlError::new("unknown-table", format!("no such table: {table}")).in_statement("alter-table"))?;
    let stored = objects
        .iter()
        .find(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&catalog.tables[position].name))
        .and_then(|object| object.sql.clone())
        .ok_or_else(|| SqlError::new("storage", "sqlite table has no stored text").in_statement("alter-table"))?;
    match action {
        AlterAction::RenameTable { new_name } => {
            check_reserved_object_name(&new_name, "alter-table")?;
            let new_key = new_name.to_ascii_lowercase();
            if catalog.table_lookup.contains_key(&new_key) || catalog.indexes.contains_key(&new_key) || catalog.views.contains_key(&new_key) {
                return Err(SqlError::new("table-exists", format!("there is already another table or index with this name: {new_name}"))
                    .in_statement("alter-table"));
            }
            let old_name = catalog.tables[position].name.clone();
            let new_sql = rewrite_create_table_name(&stored, &new_name)
                .ok_or_else(|| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
            let mut pending = Vec::new();
            for object in &objects {
                if object.kind != SchemaObjectKind::Index || !object.table_name.eq_ignore_ascii_case(&old_name) {
                    continue;
                }
                if let Some(text) = &object.sql {
                    let rewritten = rewrite_create_index_table(text, &new_name)
                        .ok_or_else(|| SqlError::new("storage", "sqlite stored index text unrecognized").in_statement("alter-table"))?;
                    pending.push((object.name.clone(), rewritten));
                }
            }
            // Every view that names the old table follows the rename, and
            // SQLite rewrites its stored text so a later reopen resolves.
            let mut pending_views = Vec::new();
            for object in &objects {
                if object.kind != SchemaObjectKind::View {
                    continue;
                }
                let Some(view) = catalog.views.get(&object.name.to_ascii_lowercase())
                else {
                    continue;
                };
                let Some(text) = &object.sql
                else {
                    continue;
                };
                if !statement_references(&view.query, &old_name) {
                    continue;
                }
                if let Some(rewritten) = rewrite_view_table(text, &old_name, &new_name) {
                    pending_views.push((view.name.clone(), rewritten));
                }
            }
            writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if let Err(error) = writer.rename_table_schema(&old_name, &new_name, &new_sql, &pending) {
                let _ = writer.rollback();
                return Err(storage_failure(error).in_statement("alter-table"));
            }
            writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if !pending_views.is_empty() {
                writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                for (name, text) in &pending_views {
                    if let Err(error) = writer.rewrite_view_sql(name, text) {
                        let _ = writer.rollback();
                        return Err(storage_failure(error).in_statement("alter-table"));
                    }
                }
                writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            }
            catalog.tables[position].name = new_name;
            reindex_tables(catalog);
            for index in catalog.indexes.values_mut() {
                if index.table == key {
                    index.table = new_key.clone();
                }
            }
            for trigger in catalog.triggers.values_mut() {
                if trigger.table == key {
                    trigger.table = new_key.clone();
                }
            }
            for (name, text) in pending_views {
                if let Some(Statement::CreateView { query, .. }) = parse(&text).ok().and_then(|statements| statements.into_iter().next()) {
                    if let Some(view) = catalog.views.get_mut(&name.to_ascii_lowercase()) {
                        view.query = *query;
                    }
                }
            }
            Ok(())
        }
        AlterAction::RenameColumn { old, new } => {
            let def = &catalog.tables[position];
            let column = def
                .columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(&old))
                .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: \"{old}\"")).in_statement("alter-table"))?;
            if def.columns.iter().enumerate().any(|(index, existing)| index != column && existing.name.eq_ignore_ascii_case(&new)) {
                return Err(SqlError::new("duplicate-column", format!("duplicate column name: {new}")).in_statement("alter-table"));
            }
            if old.eq_ignore_ascii_case(&new) {
                return Ok(());
            }
            let name = def.name.clone();
            let old_column = old;
            let new_sql = rewrite_create_table_column(&stored, &old_column, &new)
                .ok_or_else(|| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
            let mut pending = Vec::new();
            for object in &objects {
                if object.kind != SchemaObjectKind::Index || !object.table_name.eq_ignore_ascii_case(&name) {
                    continue;
                }
                if let Some(text) = &object.sql {
                    let rewritten = rewrite_create_index_column(text, &old_column, &new)
                        .ok_or_else(|| SqlError::new("storage", "sqlite stored index text unrecognized").in_statement("alter-table"))?;
                    pending.push((object.name.clone(), rewritten));
                }
            }
            // Views keep the renamed column reference spelled correctly in
            // their stored text, exactly like SQLite rewrites them.
            let mut pending_views = Vec::new();
            for object in &objects {
                if object.kind != SchemaObjectKind::View {
                    continue;
                }
                let Some(view) = catalog.views.get(&object.name.to_ascii_lowercase())
                else {
                    continue;
                };
                let Some(text) = &object.sql
                else {
                    continue;
                };
                if !statement_mentions_column(&view.query, &old_column, None) {
                    continue;
                }
                if let Some(rewritten) = rewrite_view_column(text, &old_column, &new) {
                    pending_views.push((view.name.clone(), rewritten));
                }
            }
            writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if let Err(error) = writer.rename_column_schema(&name, &new_sql, &pending) {
                let _ = writer.rollback();
                return Err(storage_failure(error).in_statement("alter-table"));
            }
            writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if !pending_views.is_empty() {
                writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                for (view, text) in &pending_views {
                    if let Err(error) = writer.rewrite_view_sql(view, text) {
                        let _ = writer.rollback();
                        return Err(storage_failure(error).in_statement("alter-table"));
                    }
                }
                writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            }
            for (view, text) in pending_views {
                if let Some(Statement::CreateView { query, .. }) = parse(&text).ok().and_then(|statements| statements.into_iter().next()) {
                    if let Some(view) = catalog.views.get_mut(&view.to_ascii_lowercase()) {
                        view.query = *query;
                    }
                }
            }
            catalog.tables[position].columns[column].name = new;
            Ok(())
        }
        AlterAction::AddColumn { column, checks, foreign_keys, sql } => {
            let def = &catalog.tables[position];
            if def.columns.iter().any(|existing| existing.name.eq_ignore_ascii_case(&column.name)) {
                return Err(SqlError::new("duplicate-column", format!("duplicate column name: {}", column.name)).in_statement("alter-table"));
            }
            if column.primary_key {
                return Err(SqlError::new("invalid-alter", "Cannot add a PRIMARY KEY column").in_statement("alter-table"));
            }
            if column.unique {
                return Err(SqlError::new("invalid-alter", "Cannot add a UNIQUE column").in_statement("alter-table"));
            }
            if !foreign_keys.is_empty() && matches!(&column.default_value, Some(value) if !matches!(value, WriteValue::Null)) {
                return Err(
                    SqlError::new("invalid-alter", "Cannot add a REFERENCES column with non-NULL default value").in_statement("alter-table")
                );
            }
            // SQLite only refuses when an existing row would read back
            // NULL; on an empty table the same definition is accepted.
            let has_rows = !writer
                .table_rows(&def.name, TableLimits::query())
                .map_err(|error| storage_failure(error).in_statement("alter-table"))?
                .is_empty();
            if column.not_null && matches!(column.default_value, None | Some(WriteValue::Null)) && has_rows {
                return Err(SqlError::new("invalid-alter", "Cannot add a NOT NULL column with default value NULL").in_statement("alter-table"));
            }
            let mut columns = def.columns.clone();
            columns.push(column.clone());
            let resolved = resolve_checks(&def.name, &columns, &checks, "alter-table", def.without_rowid)?;
            let resolved_foreign_keys = resolve_foreign_keys(&columns, &foreign_keys)?;
            let open = stored
                .rfind(')')
                .ok_or_else(|| SqlError::new("storage", "sqlite stored table text has no column list").in_statement("alter-table"))?;
            let new_sql = format!("{}, {}{}", &stored[..open], sql, &stored[open..]);
            writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if let Err(error) = writer.rewrite_table_sql(&def.name, &new_sql) {
                let _ = writer.rollback();
                return Err(storage_failure(error).in_statement("alter-table"));
            }
            writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            catalog.tables[position].columns.push(column);
            catalog.tables[position].checks.extend(resolved);
            catalog.tables[position].foreign_keys.extend(resolved_foreign_keys);
            Ok(())
        }
        AlterAction::DropColumn { column } => {
            let def = &catalog.tables[position];
            let column_position = def
                .columns
                .iter()
                .position(|existing| existing.name.eq_ignore_ascii_case(&column))
                .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: \"{column}\"")).in_statement("alter-table"))?;
            let primary = def.rowid_alias == Some(column_position)
                || def.primary_key.contains(&column_position)
                || def.columns[column_position].primary_key;
            if primary {
                return Err(SqlError::new("invalid-alter", format!("cannot drop PRIMARY KEY column: \"{column}\"")).in_statement("alter-table"));
            }
            if def.columns[column_position].unique {
                return Err(SqlError::new("invalid-alter", format!("cannot drop UNIQUE column: \"{column}\"")).in_statement("alter-table"));
            }
            if def.columns.len() == 1 {
                return Err(SqlError::new("invalid-alter", format!("cannot drop column \"{column}\": no other columns exist"))
                    .in_statement("alter-table"));
            }
            if let Some((name, _)) = catalog.indexes.iter().find(|(_, index)| index.table == key && index.columns.contains(&column_position)) {
                return Err(SqlError::new("invalid-alter", format!("error in index {name} after drop column: no such column: {column}"))
                    .in_statement("alter-table"));
            }
            let new_sql = match rewrite_create_table_drop_column(&stored, &def.columns[column_position].name) {
                Ok(Some(text)) => text,
                Ok(None) => {
                    return Err(SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"));
                }
                Err(DropColumnConflict::TableCheck) => {
                    return Err(SqlError::new(
                        "invalid-alter",
                        format!("error in table {} after drop column: no such column: {column}", def.name),
                    )
                    .in_statement("alter-table"));
                }
                Err(DropColumnConflict::TableForeignKey) => {
                    return Err(SqlError::new(
                        "invalid-alter",
                        format!("error in table {} after drop column: unknown column \"{column}\" in foreign key definition", def.name),
                    )
                    .in_statement("alter-table"));
                }
            };
            if let Some((name, _)) = catalog.views.iter().find(|(_, view)| statement_mentions_column(&view.query, &column, None)) {
                return Err(SqlError::new("invalid-alter", format!("error in view {name} after drop column: no such column: {column}"))
                    .in_statement("alter-table"));
            }
            if let Some((name, _)) = catalog.triggers.iter().find(|(_, trigger)| {
                trigger.table == key
                    && trigger.body.iter().any(|statement| statement_mentions_column(statement, &column, Some(&["new", "old"])))
            }) {
                return Err(SqlError::new("invalid-alter", format!("error in trigger {name} after drop column: no such column: {column}"))
                    .in_statement("alter-table"));
            }
            // The catalog is re-derived from the rewritten DDL so checks,
            // foreign keys and the rowid alias all follow the new column
            // positions instead of being patched by hand.
            let mut statements =
                parse(&new_sql).map_err(|_| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
            let Some(Statement::CreateTable {
                name, columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid, ..
            }) = statements.drain(..).next()
            else {
                return Err(SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"));
            };
            let derived =
                SqliteCatalog::table_entry(name, columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid)?;
            writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            if let Err(error) = writer.drop_column(&def.name, column_position) {
                let _ = writer.rollback();
                return Err(storage_failure(error).in_statement("alter-table"));
            }
            if let Err(error) = writer.rewrite_table_sql(&def.name, &new_sql) {
                let _ = writer.rollback();
                return Err(storage_failure(error).in_statement("alter-table"));
            }
            writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
            for index in catalog.indexes.values_mut() {
                if index.table != key {
                    continue;
                }
                for position in &mut index.columns {
                    if *position > column_position {
                        *position -= 1;
                    }
                }
            }
            catalog.tables[position] = derived;
            Ok(())
        }
    }
}

/// 把重建镜像写到 `target`，`VACUUM INTO` 的真实落盘步骤。
///
/// 空文件名与 `:memory:` 与上游一样不产生文件，命令仍然成功。目标已存在
/// 时不覆盖；父目录缺失、目标是目录或写入失败都报 `unable to open database`。
pub fn write_vacuum_image(target: &str, image: &[u8]) -> Result<(), SqlError> {
    if target.is_empty() || target.eq_ignore_ascii_case(":memory:") {
        return Ok(());
    }
    let unable = || SqlError::new("SQLITE_CANTOPEN", format!("unable to open database: {target}")).in_statement("vacuum");
    let path = PathBuf::from(target);
    if path.is_dir() || path.parent().is_some_and(|parent| !parent.as_os_str().is_empty() && !parent.exists()) {
        return Err(unable());
    }
    if path.exists() {
        return Err(SqlError::new("SQLITE_ERROR", "output file already exists").in_statement("vacuum"));
    }
    std::fs::write(&path, image).map_err(|_| unable())
}

/// `ATTACH` 的文件打开机器：解析路径、按需建文件并返回就绪的 writer 与
/// 登记用的 `(path, display)`。内存库返回新建 writer 与空展示名。
/// 引擎句柄构造与登记仍由门面完成。
pub fn open_attached_database(file: &str, page_size: usize) -> Result<(SqliteWriter, Option<PathBuf>, String), SqlError> {
    let memory = file.is_empty() || file.eq_ignore_ascii_case(":memory:");
    let (writer, path, display) = if memory {
        (SqliteWriter::create(page_size).map_err(|error| storage_failure(error))?, None, String::new())
    }
    else {
        let requested = PathBuf::from(file);
        let path = if requested.is_absolute() {
            requested.clone()
        }
        else {
            std::env::current_dir().map(|cwd| cwd.join(&requested)).unwrap_or_else(|_| requested.clone())
        };
        let writer = if requested.is_dir() {
            return Err(SqlError::new("unable-to-open", format!("unable to open database: {file}")).in_statement("attach"));
        }
        else if requested.exists() && std::fs::metadata(&requested).map(|meta| meta.len() > 0).unwrap_or(false) {
            SqliteWriter::open(&requested).map_err(|_| SqlError::new("not-a-database", "file is not a database").in_statement("attach"))?
        }
        else {
            // SQLite creates the file at ATTACH time but leaves it empty
            // until the first change.
            if let Some(parent) = requested.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    return Err(SqlError::new("unable-to-open", format!("unable to open database: {file}")).in_statement("attach"));
                }
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&requested)
                .map_err(|_| SqlError::new("unable-to-open", format!("unable to open database: {file}")))?;
            SqliteWriter::create(page_size).map_err(|error| storage_failure(error))?
        };
        (writer, Some(path.clone()), path.to_string_lossy().into_owned())
    };
    Ok((writer, path, display))
}
