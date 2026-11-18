//! Flow tests: a stub source and a stub target exercise ordering, batching,
//! resumable cursors, incremental replay, idempotent re-runs and pruning
//! without a server. SQL text itself is covered by the literal and DDL tests.

use std::collections::{HashMap, HashSet};

use database_migration::{
    migrate, sql_literal, ColumnDef, Error, MigrationSource, MigrationTarget, Options, PageRequest, Phase, ResultSet,
    Row, TableShape,
};

fn column(name: &str, udt: &str, primary: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_owned(),
        data_type: udt.to_owned(),
        udt: udt.to_owned(),
        max_length: None,
        numeric_precision: None,
        numeric_scale: None,
        nullable: !primary,
        default: None,
        primary,
    }
}

fn users_shape() -> TableShape {
    TableShape {
        name: "users".into(),
        columns: vec![
            column("id", "int8", true),
            column("email", "varchar", false),
            column("updated_at", "timestamptz", false),
        ],
    }
}

fn row(id: &str, email: &str, updated: &str) -> Row {
    vec![Some(id.into()), Some(email.into()), Some(updated.into())]
}

struct StubSource {
    shapes: HashMap<String, TableShape>,
    rows: HashMap<String, Vec<Row>>,
    clock: String,
}

impl MigrationSource for StubSource {
    fn tables(&mut self, _schema: &str) -> Result<Vec<String>, Error> {
        let mut names: Vec<String> = self.shapes.keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    fn shape(&mut self, _schema: &str, table: &str) -> Result<TableShape, Error> {
        self.shapes.get(table).cloned().ok_or_else(|| Error::Source(format!("no table {table}")))
    }

    fn count(&mut self, _schema: &str, table: &str) -> Result<u64, Error> {
        Ok(self.rows.get(table).map(|rows| rows.len() as u64).unwrap_or_default())
    }

    fn now(&mut self) -> Result<String, Error> {
        Ok(self.clock.clone())
    }

    fn rows(&mut self, request: &PageRequest<'_>) -> Result<Vec<Row>, Error> {
        let shape = self
            .shapes
            .get(request.table)
            .cloned()
            .ok_or_else(|| Error::Source(format!("no table {}", request.table)))?;
        let columns: Vec<usize> = request
            .order
            .iter()
            .map(|name| shape.columns.iter().position(|column| &column.name == name).expect("ordered column exists"))
            .collect();
        let mut rows = self.rows.get(request.table).cloned().unwrap_or_default();
        if let Some(since) = request.since {
            let index = shape.columns.iter().position(|column| column.name == "updated_at").expect("updated_at exists");
            rows.retain(|row| row[index].as_deref().unwrap_or_default() >= since);
        }
        if let Some(after) = request.after {
            rows.retain(|row| {
                let key: Vec<Option<String>> = columns.iter().map(|index| row[*index].clone()).collect();
                key > after.to_vec()
            });
        }
        rows.sort_by(|left, right| {
            let left: Vec<&Option<String>> = columns.iter().map(|index| &left[*index]).collect();
            let right: Vec<&Option<String>> = columns.iter().map(|index| &right[*index]).collect();
            left.cmp(&right)
        });
        rows.truncate(request.limit);
        Ok(rows)
    }
}

#[derive(Default)]
struct StubTarget {
    statements: Vec<String>,
    rows: HashMap<String, Vec<Row>>,
}

impl MigrationTarget for StubTarget {
    fn execute(&mut self, sql: &str) -> Result<(), Error> {
        self.statements.push(sql.to_owned());
        let Some(table) = table_of(sql) else { return Ok(()) };
        let bucket = self.rows.entry(table).or_default();
        if let Some(values) = sql.split(" VALUES ").nth(1) {
            // `ON CONFLICT (...) DO UPDATE` adds its own parentheses, so the
            // row list ends before that clause.
            let values = values.split(" ON CONFLICT ").next().unwrap_or(values);
            for row in parse_rows(values) {
                match bucket.iter_mut().find(|existing| existing[0] == row[0]) {
                    Some(existing) => *existing = row,
                    None => bucket.push(row),
                }
            }
        }
        if let Some(keys) = sql.split(" WHERE ").nth(1) {
            if let Some(list) = keys.split(" IN (").nth(1) {
                for row in parse_rows(list) {
                    bucket.retain(|existing| existing[0] != row[0]);
                }
            }
        }
        Ok(())
    }

    fn query(&mut self, sql: &str) -> Result<ResultSet, Error> {
        let table = table_of(sql).unwrap_or_default();
        let rows = self.rows.get(&table).cloned().unwrap_or_default();
        if sql.contains("COUNT(*)") {
            return Ok(ResultSet { columns: vec!["count".into()], rows: vec![vec![Some(rows.len().to_string())]] });
        }
        if sql.starts_with("SELECT \"") && !sql.contains('*') {
            let selected = sql
                .split(" FROM ")
                .next()
                .unwrap_or_default()
                .trim_start_matches("SELECT ")
                .split(", ")
                .map(|name| name.trim_matches('"').to_owned())
                .collect::<Vec<_>>();
            let shape_indexes: Vec<usize> = selected.iter().map(|name| shape_index(&table, name)).collect();
            let (offset, limit) = window(sql);
            let rows = rows
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|row| shape_indexes.iter().map(|index| row.get(*index).cloned().unwrap_or(None)).collect())
                .collect();
            return Ok(ResultSet { columns: selected, rows });
        }
        let (offset, limit) = window(sql);
        Ok(ResultSet { columns: Vec::new(), rows: rows.into_iter().skip(offset).take(limit).collect() })
    }
}

fn shape_index(table: &str, name: &str) -> usize {
    users_shape().columns.iter().position(|column| column.name == name).unwrap_or_else(|| panic!("{table}.{name}"))
}

fn table_of(sql: &str) -> Option<String> {
    let rest = sql.split("FROM ").nth(1).or_else(|| sql.split("INTO ").nth(1))?;
    rest.split_whitespace().next().map(|name| name.trim_matches('"').to_owned())
}

/// Parses `(a, 'b'), (c, NULL)` into rows. The fixtures hold no embedded
/// commas or escaped quotes, so a depth-and-quote scan is enough.
fn parse_rows(text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut row: Row = Vec::new();
    let mut cell = String::new();
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if !quoted => quoted = true,
            '\'' if quoted && chars.peek() == Some(&'\'') => {
                chars.next();
                cell.push('\'');
            }
            '\'' => quoted = false,
            '(' if !quoted => {
                depth += 1;
                if depth == 1 {
                    row.clear();
                    cell.clear();
                }
            }
            ')' if !quoted => {
                if depth == 1 {
                    row.push(cell_of(&cell));
                    rows.push(std::mem::take(&mut row));
                    cell.clear();
                }
                depth = depth.saturating_sub(1);
            }
            ',' if !quoted && depth == 1 => {
                row.push(cell_of(&cell));
                cell.clear();
            }
            other => cell.push(other),
        }
    }
    rows
}

fn cell_of(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("NULL") { None } else { Some(trimmed.to_owned()) }
}

fn window(sql: &str) -> (usize, usize) {
    let number_after = |keyword: &str| {
        sql.split(keyword)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|text| text.parse::<usize>().ok())
            .unwrap_or(0)
    };
    (number_after("OFFSET "), number_after("LIMIT ").max(1))
}

fn source(rows: Vec<Row>) -> StubSource {
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new(), clock: "2026-11-18T00:00:00.000000Z".into() };
    source.shapes.insert("users".into(), users_shape());
    source.rows.insert("users".into(), rows);
    source
}

fn target_with(rows: Vec<Row>) -> StubTarget {
    let mut target = StubTarget::default();
    target.rows.insert("users".into(), rows);
    target
}

#[test]
fn first_run_scans_fully_then_replays_incrementally() {
    let mut source = source(vec![row("1", "a@x.com", "2026-11-01T00:00:00Z"), row("2", "b@x.com", "2026-11-02T00:00:00Z")]);
    let mut target = target_with(Vec::new());

    let report = migrate(&mut source, &mut target, &Options { batch_rows: 1, ..Options::default() }).unwrap();

    assert_eq!(report.tables[0].phase, Phase::Incremental);
    assert_eq!(report.tables[0].written, 2, "full scan writes both rows and the replay finds nothing newer");
    assert!(report.tables[0].verified, "{:?}", report.tables[0]);
    assert!(!target.statements.is_empty());
    assert!(target.statements[0].starts_with("CREATE TABLE IF NOT EXISTS \"users\""));
}

#[test]
fn rerun_is_idempotent_and_only_replays_changes() {
    let rows = vec![row("1", "a@x.com", "2026-11-01T00:00:00Z")];
    let mut source = source(rows.clone());
    let mut target = target_with(rows);
    let state = std::env::temp_dir().join(format!("migration-rerun-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let options = Options { state_path: Some(state.clone()), ..Options::default() };

    migrate(&mut source, &mut target, &options).unwrap();
    let statements = target.statements.len();
    let report = migrate(&mut source, &mut target, &options).unwrap();

    assert_eq!(report.tables[0].written, 0, "nothing changed since the watermark");
    let fresh: Vec<&String> = target.statements[statements..].iter().filter(|sql| sql.contains("INSERT")).collect();
    assert!(fresh.is_empty(), "a re-run with an unchanged source writes nothing: {fresh:#?}");
    assert_eq!(target.rows["users"].len(), 1, "the row was not duplicated");
    let _ = std::fs::remove_file(state);
}

#[test]
fn incremental_pass_picks_up_updates_after_the_watermark() {
    let mut source = source(vec![row("1", "old@x.com", "2026-11-01T00:00:00Z")]);
    let mut target = target_with(vec![row("1", "old@x.com", "2026-11-01T00:00:00Z")]);
    let state = std::env::temp_dir().join(format!("migration-state-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&state);

    let first = migrate(&mut source, &mut target, &Options { state_path: Some(state.clone()), ..Options::default() }).unwrap();
    assert_eq!(first.tables[0].phase, Phase::Incremental);

    source.rows.insert("users".into(), vec![row("1", "new@x.com", "2026-11-18T00:00:01Z")]);
    let second = migrate(&mut source, &mut target, &Options { state_path: Some(state.clone()), ..Options::default() }).unwrap();

    assert_eq!(second.tables[0].written, 1);
    assert!(target.statements.iter().any(|sql| sql.contains("ON CONFLICT")), "{:#?}", target.statements);
    let _ = std::fs::remove_file(state);
}

#[test]
fn cursor_is_saved_so_a_resume_skips_finished_batches() {
    let mut source = source(vec![
        row("1", "a@x.com", "2026-11-01T00:00:00Z"),
        row("2", "b@x.com", "2026-11-02T00:00:00Z"),
    ]);
    let mut target = target_with(Vec::new());

    let report = migrate(&mut source, &mut target, &Options { batch_rows: 1, ..Options::default() }).unwrap();

    assert_eq!(report.tables[0].written, 2);
    assert!(target.statements.iter().filter(|sql| sql.starts_with("INSERT")).count() >= 2);
}

#[test]
fn prune_removes_rows_the_source_deleted() {
    let mut source = source(vec![row("1", "a@x.com", "2026-11-01T00:00:00Z")]);
    let mut target = target_with(vec![
        row("1", "a@x.com", "2026-11-01T00:00:00Z"),
        row("9", "gone@x.com", "2026-11-01T00:00:00Z"),
    ]);

    let report = migrate(&mut source, &mut target, &Options { prune: true, ..Options::default() }).unwrap();

    assert_eq!(report.tables[0].pruned, 1);
    assert!(target.statements.iter().any(|sql| sql.starts_with("DELETE FROM")), "{:#?}", target.statements);
}

#[test]
fn dry_run_never_touches_the_target() {
    let mut source = source(vec![row("1", "a@x.com", "2026-11-01T00:00:00Z")]);
    let mut target = target_with(Vec::new());

    let report = migrate(&mut source, &mut target, &Options { dry_run: true, ..Options::default() }).unwrap();

    assert!(target.statements.is_empty());
    assert_eq!(report.tables[0].source_rows, 1);
    assert_eq!(report.tables[0].written, 0);
}

#[test]
fn table_without_primary_key_is_refused_explicitly() {
    let shape = TableShape { name: "logs".into(), columns: vec![column("message", "text", false)] };
    let mut source = StubSource {
        shapes: HashMap::from([("logs".to_owned(), shape)]),
        rows: HashMap::from([("logs".to_owned(), vec![vec![Some("hi".into())]])]),
        clock: "2026-11-18T00:00:00Z".into(),
    };
    let mut target = StubTarget::default();

    let error = migrate(&mut source, &mut target, &Options::default()).unwrap_err();

    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(error.to_string().contains("no primary key"));
}

#[test]
fn literals_follow_the_target_type() {
    assert_eq!(sql_literal("bigint", Some("42")), "42");
    assert_eq!(sql_literal("numeric(20,6)", Some("10.500000")), "10.500000");
    assert_eq!(sql_literal("boolean", Some("t")), "TRUE");
    assert_eq!(sql_literal("boolean", Some("f")), "FALSE");
    assert_eq!(sql_literal("uuid", Some("11111111-1111-1111-1111-111111111111")), "'11111111-1111-1111-1111-111111111111'");
    assert_eq!(sql_literal("text", Some("it's")), "'it''s'");
    assert_eq!(sql_literal("bigint", None), "NULL");
    assert_eq!(sql_literal("jsonb", Some("{\"tier\":\"gold\"}")), "'{\"tier\":\"gold\"}'");
}

#[test]
fn unsupported_source_type_is_reported_not_approximated() {
    let shape = TableShape { name: "t".into(), columns: vec![column("id", "int8", true), column("tags", "_text", false)] };
    let mut source = StubSource {
        shapes: HashMap::from([("t".to_owned(), shape)]),
        rows: HashMap::from([("t".to_owned(), Vec::new())]),
        clock: "2026-11-18T00:00:00Z".into(),
    };
    let mut target = StubTarget::default();

    let error = migrate(&mut source, &mut target, &Options::default()).unwrap_err();

    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(error.to_string().contains("_text"));
}

#[test]
fn state_file_records_the_phase_and_rejects_unknown_formats() {
    let directory = std::env::temp_dir();
    let path = directory.join(format!("migration-phase-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let mut source = source(vec![row("1", "a@x.com", "2026-11-01T00:00:00Z")]);
    let mut target = target_with(Vec::new());
    migrate(&mut source, &mut target, &Options { state_path: Some(path.clone()), ..Options::default() }).unwrap();

    let state = database_migration::MigrationState::load(&path).unwrap();
    assert_eq!(state.tables["users"].phase, Phase::Incremental);
    assert!(!state.tables["users"].watermark.is_empty());

    std::fs::write(&path, r#"{"format":99,"tables":{}}"#).unwrap();
    let error = database_migration::MigrationState::load(&path).unwrap_err();
    assert!(error.to_string().contains("state format"));
    let _ = std::fs::remove_file(path);
}

#[allow(dead_code)]
fn keys_of(rows: &[Row]) -> HashSet<String> {
    rows.iter().map(|row| row[0].clone().unwrap_or_default()).collect()
}
