//! Flow tests: a stub source drives a stub target so ordering, batching and
//! verification are exercised without a server. SQL semantics themselves are
//! covered by the DDL and literal unit tests below.

use std::collections::HashMap;

use database_migration::{
    migrate, sql_literal, ColumnDef, Error, MigrationSource, MigrationTarget, Options, ResultSet, TableShape,
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

struct StubSource {
    shapes: HashMap<String, TableShape>,
    rows: HashMap<String, Vec<Vec<Option<String>>>>,
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

    fn rows(
        &mut self,
        _schema: &str,
        table: &str,
        _order: &Option<Vec<String>>,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<Vec<Option<String>>>, Error> {
        let rows = self.rows.get(table).cloned().unwrap_or_default();
        Ok(rows.into_iter().skip(offset as usize).take(limit).collect())
    }
}

#[derive(Default)]
struct StubTarget {
    statements: Vec<String>,
    rows: HashMap<String, Vec<Vec<Option<String>>>>,
}

impl MigrationTarget for StubTarget {
    fn execute(&mut self, sql: &str) -> Result<(), Error> {
        self.statements.push(sql.to_owned());
        Ok(())
    }

    fn query(&mut self, sql: &str) -> Result<ResultSet, Error> {
        let table = sql
            .split("FROM ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(|name| name.trim_matches('"').to_owned())
            .unwrap_or_default();
        let rows = self.rows.get(&table).cloned().unwrap_or_default();
        if sql.contains("COUNT(*)") {
            return Ok(ResultSet { columns: vec!["count".into()], rows: vec![vec![Some(rows.len().to_string())]] });
        }
        let (offset, limit) = window(sql);
        Ok(ResultSet {
            columns: Vec::new(),
            rows: rows.into_iter().skip(offset).take(limit).collect(),
        })
    }
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

#[test]
fn copies_every_table_in_name_order_and_verifies() {
    let users = TableShape {
        name: "users".into(),
        columns: vec![column("id", "int8", true), column("email", "varchar", false)],
    };
    let orders = TableShape {
        name: "orders".into(),
        columns: vec![column("id", "int8", true), column("amount", "numeric", false)],
    };
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new() };
    source.shapes.insert("users".into(), users);
    source.shapes.insert("orders".into(), orders);
    source.rows.insert(
        "users".into(),
        vec![vec![Some("1".into()), Some("a@x.com".into())], vec![Some("2".into()), None]],
    );
    source.rows.insert("orders".into(), vec![vec![Some("1".into()), Some("10.5".into())]]);

    let mut target = StubTarget::default();
    target.rows.insert("users".into(), source.rows["users"].clone());
    target.rows.insert("orders".into(), source.rows["orders"].clone());

    let report = migrate(&mut source, &mut target, &Options { batch_rows: 1, ..Options::default() }).unwrap();

    assert_eq!(report.tables.iter().map(|table| table.name.as_str()).collect::<Vec<_>>(), ["orders", "users"]);
    assert_eq!(report.total_rows(), 3);
    assert!(report.fully_verified(), "{report:?}");
    assert_eq!(target.statements.len(), 5, "two DDLs plus one insert per row window: {:#?}", target.statements);
    assert!(target.statements[0].starts_with("CREATE TABLE \"orders\""));
    assert!(target.statements[1].contains("(1, 10.5)"));
}

#[test]
fn batches_larger_than_one_stay_in_one_insert() {
    let shape = TableShape { name: "t".into(), columns: vec![column("id", "int8", true)] };
    let rows: Vec<Vec<Option<String>>> = (0..3).map(|index| vec![Some(index.to_string())]).collect();
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new() };
    source.shapes.insert("t".into(), shape);
    source.rows.insert("t".into(), rows.clone());
    let mut target = StubTarget::default();
    target.rows.insert("t".into(), rows);

    let report = migrate(&mut source, &mut target, &Options::default()).unwrap();

    assert!(report.fully_verified());
    let inserts: Vec<&String> = target.statements.iter().filter(|sql| sql.starts_with("INSERT")).collect();
    assert_eq!(inserts.len(), 1);
    assert_eq!(inserts[0], "INSERT INTO \"t\" (\"id\") VALUES (0), (1), (2)");
}

#[test]
fn table_without_primary_key_is_copied_but_not_verified() {
    let shape = TableShape { name: "logs".into(), columns: vec![column("message", "text", false)] };
    let rows = vec![vec![Some("hello".into())]];
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new() };
    source.shapes.insert("logs".into(), shape);
    source.rows.insert("logs".into(), rows.clone());
    let mut target = StubTarget::default();
    target.rows.insert("logs".into(), rows);

    let report = migrate(&mut source, &mut target, &Options::default()).unwrap();

    assert_eq!(report.tables[0].target_rows, 1);
    assert!(!report.tables[0].verified);
    assert!(report.tables[0].note.as_deref().unwrap().contains("no primary key"));
}

#[test]
fn dry_run_never_touches_the_target() {
    let shape = TableShape { name: "t".into(), columns: vec![column("id", "int8", true)] };
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new() };
    source.shapes.insert("t".into(), shape);
    source.rows.insert("t".into(), vec![vec![Some("1".into())]]);
    let mut target = StubTarget::default();

    let report = migrate(&mut source, &mut target, &Options { dry_run: true, ..Options::default() }).unwrap();

    assert!(target.statements.is_empty());
    assert_eq!(report.tables[0].source_rows, 1);
    assert_eq!(report.tables[0].target_rows, 0);
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
    let shape = TableShape { name: "t".into(), columns: vec![column("tags", "_text", false)] };
    let mut source = StubSource { shapes: HashMap::new(), rows: HashMap::new() };
    source.shapes.insert("t".into(), shape);
    source.rows.insert("t".into(), Vec::new());
    let mut target = StubTarget::default();

    let error = migrate(&mut source, &mut target, &Options::default()).unwrap_err();

    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(error.to_string().contains("_text"));
}
