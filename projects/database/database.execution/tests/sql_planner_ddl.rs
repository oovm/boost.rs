//! DDL → catalog 投影与计划器对注册表 catalog 的真实消费。
//!
//! 这些用例需要 `database_execution::sql_schema::SqlSchemaRegistry` 应用 DDL 后才
//! 能构造 catalog，因此随执行侧注册表放在 `database-execution`，只消费
//! `database-query` 的公开计划器 API。

use database_execution::sql_schema::SqlSchemaRegistry;
use database_query::{
    SqlCatalog, SqlCatalogBoundExpression, SqlCatalogBoundStatement, SqlDialect, SqlErrorKind, SqlExecutionPlan, SqlFieldType, SqlFrontendError,
    bind_sql_catalog,
    sql::SqlBoundBinaryOperator,
    sql_ddl::parse_ddl,
    sql_index::index_namespace,
    sql_planner::{ScanPlan, UpdateAssignment, plan_insert, plan_statement_for_dialect},
};

fn apply_ddl(registry: &SqlSchemaRegistry, sql: &str) {
    let statement = parse_ddl(sql, SqlDialect::Mysql).expect("parse").expect("DDL statement");
    registry.apply_ddl(&statement).expect("apply DDL");
}

fn mysql_catalog(ddl: &str) -> SqlCatalog {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply_ddl(&registry, ddl);
    registry.catalog()
}

fn plan_mysql_insert(registry: &SqlSchemaRegistry, sql: &str) -> Result<SqlExecutionPlan, SqlFrontendError> {
    let catalog = registry.catalog();
    let SqlCatalogBoundStatement::Insert(insert) = bind_sql_catalog(sql, &catalog, catalog.schema_version(), SqlDialect::Mysql).expect("bind")
    else {
        panic!("expected INSERT");
    };
    plan_insert(insert, &catalog, SqlDialect::Mysql)
}

fn plan_mysql(catalog: &SqlCatalog, sql: &str) -> SqlExecutionPlan {
    let bound = bind_sql_catalog(sql, catalog, catalog.schema_version(), SqlDialect::Mysql).expect("bind");
    plan_statement_for_dialect(bound, catalog, SqlDialect::Mysql).expect("plan")
}

fn bind_and_plan_err(catalog: &SqlCatalog, sql: &str) -> SqlFrontendError {
    let bound = bind_sql_catalog(sql, catalog, catalog.schema_version(), SqlDialect::Mysql).expect("bind");
    plan_statement_for_dialect(bound, catalog, SqlDialect::Mysql).expect_err("plan must fail")
}

#[test]
fn omitted_not_null_column_without_default_reports_missing_default() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply_ddl(&registry, "CREATE TABLE truth_err (id INT NOT NULL PRIMARY KEY, v INT NOT NULL)");
    assert!(plan_mysql_insert(&registry, "INSERT INTO truth_err (id, v) VALUES (1, 5)").is_ok());
    let error = plan_mysql_insert(&registry, "INSERT INTO truth_err (id) VALUES (4)").expect_err("missing NOT NULL column must fail");
    assert_eq!(error.kind, SqlErrorKind::MissingDefault);
    assert_eq!(error.message, "Field 'v' doesn't have a default value");
}

#[test]
fn omitted_nullable_or_defaulted_column_still_plans() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply_ddl(&registry, "CREATE TABLE truth_ok (id INT NOT NULL PRIMARY KEY, note INT, tag VARCHAR(8) NOT NULL DEFAULT 'x')");
    assert!(plan_mysql_insert(&registry, "INSERT INTO truth_ok (id) VALUES (1)").is_ok());
}

#[test]
fn date_and_datetime_ddl_project_their_own_field_types() {
    let catalog = mysql_catalog("CREATE TABLE truth_types (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME, ts TIMESTAMP NULL)");
    let table = catalog.tables().iter().find(|table| table.name == "truth_types").expect("table");
    let field_type = |name: &str| table.fields.iter().find(|field| field.name == name).expect("field").field_type.inner();
    assert_eq!(field_type("d"), SqlFieldType::Date);
    assert_eq!(field_type("dt"), SqlFieldType::Datetime);
    assert_eq!(field_type("ts"), SqlFieldType::Datetime);
}

#[test]
fn decimal_ddl_projects_the_decimal_field_type() {
    let catalog = mysql_catalog("CREATE TABLE truth_dec (id INT NOT NULL PRIMARY KEY, price DECIMAL(10,2))");
    let table = catalog.tables().iter().find(|table| table.name == "truth_dec").expect("table");
    let price = table.fields.iter().find(|field| field.name == "price").expect("field");
    assert_eq!(price.field_type.inner(), SqlFieldType::Decimal);
}

/// 整数主键的范围条件不得变成 KV 字节边界：`rowid_after` / `rowid_before` 必须为空，
/// 条件整体留在 remaining 行级过滤。`BETWEEN` 脱糖成 `>= AND <=`，同样留在 remaining。
#[test]
fn integer_pk_range_stays_row_level_without_byte_bounds() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)");
    for sql in [
        "SELECT id FROM t WHERE id > 6",
        "SELECT id FROM t WHERE id >= 50",
        "SELECT id FROM t WHERE id < 10",
        "SELECT id FROM t WHERE id <= 10",
        "SELECT id FROM t WHERE id BETWEEN 6 AND 46",
        "SELECT id FROM t WHERE id > 6 AND id < 46",
    ] {
        let SqlExecutionPlan::Scan(scan) = plan_mysql(&catalog, sql) else {
            panic!("`{sql}` must plan to a scan")
        };
        assert!(scan.rowid_after.is_none(), "`{sql}` must not push a byte lower bound");
        assert!(scan.rowid_before.is_none(), "`{sql}` must not push a byte upper bound");
        assert!(scan.remaining_predicate.is_some(), "`{sql}` must keep the range as a row filter");
        // remaining 谓词存在时 kv-scan 不能带 limit，否则字节序截断仍会丢行。
        assert_eq!(scan.limit, 0, "`{sql}` must scan without a pushed-down limit");
    }
}

/// UPDATE / DELETE 的范围谓词同样留在行级：扫描边界为空，执行层按 predicate 逐行过滤。
#[test]
fn update_and_delete_range_stays_row_level() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)");
    let SqlExecutionPlan::UpdateScan(update) = plan_mysql(&catalog, "UPDATE t SET v = 1 WHERE id > 6") else {
        panic!("UPDATE must scan")
    };
    assert!(update.rowid_after.is_none() && update.rowid_before.is_none());
    assert!(update.predicate.is_some(), "UPDATE must keep the range as a row filter");
    let SqlExecutionPlan::DeleteScan(delete) = plan_mysql(&catalog, "DELETE FROM t WHERE id >= 50") else {
        panic!("DELETE must scan")
    };
    assert!(delete.rowid_after.is_none() && delete.rowid_before.is_none());
    assert!(delete.predicate.is_some(), "DELETE must keep the range as a row filter");
}

/// 等值主键仍走单点读：rowid 是主键字面量的规范字节。
#[test]
fn pk_equality_still_reads_one_rowid_point() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)");
    let SqlExecutionPlan::Read(read) = plan_mysql(&catalog, "SELECT id FROM t WHERE id = 42") else {
        panic!("equality must plan to a point read")
    };
    assert_eq!(read.rowid, b"42".to_vec());
    assert!(read.remaining_predicate.is_none());
}

/// 等值 + 其他谓词：点读保留，其余条件留在 remaining。
#[test]
fn pk_equality_with_conjunct_keeps_remaining_filter() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)");
    let SqlExecutionPlan::Read(read) = plan_mysql(&catalog, "SELECT id FROM t WHERE id = 42 AND v > 1") else {
        panic!("equality must plan to a point read")
    };
    assert_eq!(read.rowid, b"42".to_vec());
    assert!(read.remaining_predicate.is_some());
}

/// 字符串主键的前缀 LIKE 仍下推为字节前缀扫描。
#[test]
fn string_pk_like_prefix_still_pushes_down() {
    let catalog = mysql_catalog("CREATE TABLE s (name TEXT NOT NULL PRIMARY KEY, v INT)");
    let SqlExecutionPlan::Scan(scan) = plan_mysql(&catalog, "SELECT name FROM s WHERE name LIKE 'abc%'") else {
        panic!("LIKE prefix must plan to a scan")
    };
    assert_eq!(scan.rowid_prefix, b"abc".to_vec());
    assert!(scan.remaining_predicate.is_none(), "prefix LIKE is fully pushed down");
}

/// 非纯前缀的 LIKE 模式不下推，留在 remaining 行级求值。
#[test]
fn like_pattern_beyond_prefix_stays_row_level() {
    let catalog = mysql_catalog("CREATE TABLE s (name TEXT NOT NULL PRIMARY KEY, v INT)");
    let SqlExecutionPlan::Scan(scan) = plan_mysql(&catalog, "SELECT name FROM s WHERE name LIKE 'a%c'") else {
        panic!("LIKE must plan to a scan")
    };
    assert!(scan.rowid_prefix.is_empty());
    assert!(scan.remaining_predicate.is_some());
}

/// UPDATE 的 SET 右值携带绑定后的行表达式；纯字面量右值仍是 Literal 并保持计划期校验。
#[test]
fn update_set_carries_bound_row_expressions() {
    let catalog = mysql_catalog("CREATE TABLE inv (id INT NOT NULL PRIMARY KEY, stock INT, name TEXT)");
    let SqlExecutionPlan::Update(update) = plan_mysql(&catalog, "UPDATE inv SET stock = stock - 1 WHERE id = 1") else {
        panic!("point update")
    };
    let UpdateAssignment { field_name, value, .. } = &update.fields[0];
    assert_eq!(field_name, "stock");
    let SqlCatalogBoundExpression::Binary { operator, .. } = value else {
        panic!("arithmetic RHS must stay a Binary expression")
    };
    assert_eq!(*operator, SqlBoundBinaryOperator::Minus);
    // 字面量右值保持原形状。
    let SqlExecutionPlan::Update(literal_update) = plan_mysql(&catalog, "UPDATE inv SET stock = 5 WHERE id = 1") else {
        panic!("point update")
    };
    assert!(matches!(&literal_update.fields[0].value, SqlCatalogBoundExpression::Literal(database_query::SqlBoundLiteral::Integer(5))));
    // 字面量的类型校验仍在计划期报错。
    let error = bind_and_plan_err(&catalog, "UPDATE inv SET stock = 'abc' WHERE id = 1");
    assert_eq!(error.kind, SqlErrorKind::TypeMismatch);
}

fn scan_plan(catalog: &SqlCatalog, sql: &str) -> ScanPlan {
    let plan = plan_mysql(catalog, sql);
    match plan {
        SqlExecutionPlan::Scan(scan) => scan,
        other => panic!("`{sql}` must plan to a scan, got {other:?}"),
    }
}

fn field_id(catalog: &SqlCatalog, table: &str, column: &str) -> u64 {
    catalog
        .tables()
        .iter()
        .find(|candidate| candidate.name == table)
        .and_then(|candidate| candidate.fields.iter().find(|field| field.name == column))
        .map(|field| field.field_id)
        .expect("field")
}

fn index_namespace_of(catalog: &SqlCatalog, table: &str, column: &str) -> String {
    let table = catalog.tables().iter().find(|candidate| candidate.name == table).expect("table");
    index_namespace(&format!("table:{}", table.type_id), field_id(catalog, table.name.as_str(), column))
}

/// `uk = literal` 生成一次索引区间探测（等值即同一值的条目前缀），同时保留原谓词做行级复核。
#[test]
fn unique_equality_probes_the_index() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, uk INT UNIQUE, tag VARCHAR(8) UNIQUE)");
    let scan = scan_plan(&catalog, "SELECT id FROM t WHERE uk = 7");
    let probe = scan.index.expect("unique equality must probe the index");
    assert_eq!(probe.namespace, index_namespace_of(&catalog, "t", "uk"));
    assert!(probe.after.is_some() && probe.before.is_some(), "equality must bound the value prefix");
    assert!(scan.remaining_predicate.is_some(), "the predicate must still be rechecked per row");

    // 文本唯一列的等值同样可以走索引。
    let scan = scan_plan(&catalog, "SELECT id FROM t WHERE tag = 'abc'");
    let probe = scan.index.expect("text unique equality must probe the index");
    assert_eq!(probe.namespace, index_namespace_of(&catalog, "t", "tag"));
    assert!(probe.after.is_some() && probe.before.is_some());
}

/// `BETWEEN` 脱糖成 `>= AND <=`，整数列翻译成索引范围游标。
#[test]
fn unique_between_becomes_a_range_scan() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, uk INT UNIQUE)");
    let scan = scan_plan(&catalog, "SELECT id FROM t WHERE uk BETWEEN 10 AND 20");
    let probe = scan.index.expect("unique range must scan the index");
    assert!(probe.after.is_some(), "inclusive lower bound must set a cursor");
    assert!(probe.before.is_some(), "inclusive upper bound must set a cursor");

    // 单边范围同样只设置对应一侧。
    let scan = scan_plan(&catalog, "SELECT id FROM t WHERE uk > 10");
    let probe = scan.index.expect("one-sided range must scan the index");
    assert!(probe.after.is_some() && probe.before.is_none());
}

/// 只有顶层 `AND` 合取项能圈候选集；`OR` 一侧与文本范围都不走索引。
#[test]
fn disjunctions_and_text_ranges_fall_back_to_a_full_scan() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, uk INT UNIQUE, tag VARCHAR(8) UNIQUE)");
    assert!(scan_plan(&catalog, "SELECT id FROM t WHERE uk = 1 OR uk = 2").index.is_none());
    assert!(scan_plan(&catalog, "SELECT id FROM t WHERE tag BETWEEN 'a' AND 'b'").index.is_none());
    // 没有唯一约束的列不建索引。
    let catalog = mysql_catalog("CREATE TABLE plain (id INT NOT NULL PRIMARY KEY, v INT)");
    assert!(scan_plan(&catalog, "SELECT id FROM plain WHERE v = 1").index.is_none());
}

/// 多列唯一约束不在本轮范围内，保持整表扫描。
#[test]
fn multi_column_unique_keys_are_not_indexed() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, a INT, b INT, UNIQUE KEY pair (a, b))");
    assert!(scan_plan(&catalog, "SELECT id FROM t WHERE a = 1").index.is_none());
}

/// 主键 LIKE 前缀已被抽成 rowid 前缀扫描（remaining 里不再有它），此时不能用
/// 二级索引，否则候选集会绕开前缀条件。
#[test]
fn primary_key_like_prefix_disables_the_secondary_probe() {
    let catalog = mysql_catalog("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, uk INT UNIQUE)");
    let scan = scan_plan(&catalog, "SELECT id FROM t WHERE id LIKE '1%' AND uk = 7");
    assert!(!scan.rowid_prefix.is_empty());
    assert!(scan.index.is_none(), "rowid prefix is semantic, the index probe would lose it");
}
