//! 集群 KV 执行器之上的 SQL 网关端到端回归。
//!
//! 钉死两条合同：
//! 1. 整数主键的范围条件（`> >= < <= BETWEEN`）按数值语义过滤，绝不因 rowid 的
//!    十进制 ASCII 字节序而丢行或多行（历史缺陷：`id >= 50` 曾返回 55 行）。
//! 2. `UPDATE ... SET` 右值可以是行表达式（算术、列引用、标量函数），逐行求值。
//!
//! 执行面用 `FileShard`：多行 `INSERT` 与多行 `UPDATE/DELETE` 的原子提交路径要求
//! store 提供 durable 原子提交边界，`MemoryShard` 对该边界返回 `Unsupported`。

use std::{collections::HashMap, fs, path::PathBuf};

use database_cluster::topology::ShardMap;
use database_execution::{cluster::LocalExecutor, sql_exec::execute_plan, sql_schema::SqlSchemaRegistry};
use database_query::{
    SqlCatalogBoundStatement, SqlResult, SqlValue, bind_sql_catalog, parse_ddl, plan_statement_for_dialect,
    sql::{SqlDialect, SqlFrontendError},
};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

struct Gateway {
    registry: SqlSchemaRegistry,
    executor: LocalExecutor<FileShard>,
    directory: PathBuf,
}

impl Gateway {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("database-execution-sql-gateway-{name}-{}", std::process::id()));
        let _ = fs::remove_file(directory.join("local-0.yykv"));
        fs::create_dir_all(&directory).unwrap();
        let shard = ShardId("local-0".into());
        let executor = LocalExecutor::new(
            ShardMap::new(ShardEpoch(1), vec![shard.clone()]).unwrap(),
            HashMap::from([(shard, FileShard::open(directory.join("local-0.yykv")).unwrap())]),
            Vec::new(),
        )
        .unwrap();
        Self { registry: SqlSchemaRegistry::new_in_memory(), executor, directory }
    }

    /// 执行一条 SQL：DDL 走 schema registry，其余走 bind → plan → execute。
    fn sql(&self, text: &str) -> Result<SqlResult, SqlFrontendError> {
        if let Some(statement) = parse_ddl(text, SqlDialect::Mysql)? {
            self.registry.apply_ddl(&statement)?;
            return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 0 });
        }
        let catalog = self.registry.catalog();
        let bound = bind_sql_catalog(text, &catalog, catalog.schema_version(), SqlDialect::Mysql)?;
        let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Mysql)?;
        execute_plan(&self.executor, None, &plan, SqlDialect::Mysql)
    }

    fn expect_sql(&self, text: &str) -> SqlResult {
        self.sql(text).unwrap_or_else(|error| panic!("`{text}` failed: {}", error.message))
    }

    /// 单列单行查询的整数值，用于 `COUNT(*)` 与点查断言。
    fn scalar_integer(&self, text: &str) -> i64 {
        let result = self.expect_sql(text);
        assert_eq!(result.rows.len(), 1, "`{text}` must return exactly one row");
        match &result.rows[0][0] {
            SqlValue::Integer(value) => *value,
            other => panic!("`{text}` expected an integer, got {other:?}"),
        }
    }

    fn scalar_text(&self, text: &str) -> String {
        let result = self.expect_sql(text);
        assert_eq!(result.rows.len(), 1, "`{text}` must return exactly one row");
        match &result.rows[0][0] {
            SqlValue::String(value) => value.clone(),
            other => panic!("`{text}` expected a string, got {other:?}"),
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("local-0.yykv"));
        let _ = fs::remove_dir(&self.directory);
    }
}

/// 建一张 id 为 1..=100 的整数主键表。
fn gateway_with_hundred_rows(name: &str) -> Gateway {
    let gateway = Gateway::new(name);
    gateway.expect_sql("CREATE TABLE nums (id INT NOT NULL PRIMARY KEY, v INT)");
    let values: Vec<String> = (1..=100).map(|id| format!("({id}, {id})")).collect();
    gateway.expect_sql(&format!("INSERT INTO nums (id, v) VALUES {}", values.join(", ")));
    gateway
}

/// 整数主键的范围条件必须按数值序过滤：字节序下推曾把 `"55" > "49"` 当成 `55 >= 50`，
/// 把 `"6" > "46"` 当成 `6 > 46`。每个期望值都是真库数值语义的行数。
#[test]
fn integer_primary_key_ranges_filter_by_numeric_order() {
    let gateway = gateway_with_hundred_rows("pk-range");
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id > 6"), 94);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id >= 50"), 51);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id BETWEEN 6 AND 46"), 41);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id BETWEEN 2 AND 10"), 9);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id < 10"), 9);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id <= 10"), 10);
}

/// 等值主键仍走单点读；范围条件命中多行时走扫描 + 行级过滤，两者行数一致。
#[test]
fn equality_point_read_and_range_scan_agree() {
    let gateway = gateway_with_hundred_rows("pk-equality");
    assert_eq!(gateway.scalar_integer("SELECT v FROM nums WHERE id = 42"), 42);
    assert_eq!(gateway.scalar_integer("SELECT v FROM nums WHERE id = 100"), 100);
    // 扫描路径逐行返回的行数与数值谓词一致（旧字节边界会漏掉一位数 id）。
    let result = gateway.expect_sql("SELECT id FROM nums WHERE id < 10");
    assert_eq!(result.rows.len(), 9);
    let result = gateway.expect_sql("SELECT id FROM nums WHERE id >= 50");
    assert_eq!(result.rows.len(), 51);
}

/// UPDATE / DELETE 移除字节边界下推后仍按行级谓词命中：范围条件影响真实数值行。
#[test]
fn update_and_delete_range_hits_numeric_rows() {
    let gateway = gateway_with_hundred_rows("pk-dml");
    // `v = -1` 是哨兵值：插入数据 v = 1..100，命中行数只可能来自谓词本身。
    assert_eq!(gateway.expect_sql("UPDATE nums SET v = -1 WHERE id BETWEEN 2 AND 10").affected_rows, 9);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE v = -1"), 9);
    assert_eq!(gateway.expect_sql("DELETE FROM nums WHERE id >= 50").affected_rows, 51);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums"), 49);
    assert_eq!(gateway.expect_sql("DELETE FROM nums WHERE id < 10").affected_rows, 9);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums"), 40);
}

/// 字符串主键不受本次修复影响：等值点读与前缀 LIKE 扫描都保持原语义。
#[test]
fn string_primary_key_equality_and_like_prefix_still_work() {
    let gateway = Gateway::new("string-pk");
    gateway.expect_sql("CREATE TABLE words (name TEXT NOT NULL PRIMARY KEY, v INT)");
    gateway.expect_sql("INSERT INTO words (name, v) VALUES ('apple', 1), ('apricot', 2), ('banana', 3)");
    assert_eq!(gateway.scalar_integer("SELECT v FROM words WHERE name = 'apple'"), 1);
    let result = gateway.expect_sql("SELECT name FROM words WHERE name LIKE 'ap%'");
    assert_eq!(result.rows.len(), 2);
    let result = gateway.expect_sql("SELECT name FROM words WHERE name LIKE 'a%e'");
    assert_eq!(result.rows.len(), 1);
    // 文本主键不做十进制字节边界下推：范围仍按文本比较语义过滤。
    // `apricot` 与 `banana` 都落在 (apple, banana] 内。
    let result = gateway.expect_sql("SELECT name FROM words WHERE name > 'apple' AND name <= 'banana'");
    assert_eq!(result.rows.len(), 2);
}

/// `UPDATE ... SET` 右值支持算术表达式：点读路径与扫描路径都按当前行值求值。
#[test]
fn update_set_arithmetic_evaluates_per_row() {
    let gateway = Gateway::new("update-arith");
    gateway.expect_sql("CREATE TABLE inv (id INT NOT NULL PRIMARY KEY, stock INT, price INT, name TEXT)");
    gateway.expect_sql("INSERT INTO inv (id, stock, price, name) VALUES (1, 10, 2, 'a'), (2, 5, 3, 'b')");

    // 点读路径：`SET stock = stock - 1 WHERE id = 1`。
    assert_eq!(gateway.expect_sql("UPDATE inv SET stock = stock - 1 WHERE id = 1").affected_rows, 1);
    assert_eq!(gateway.scalar_integer("SELECT stock FROM inv WHERE id = 1"), 9);

    // 扫描路径：算术含乘法优先级 `SET stock = stock + price * 2`。
    assert_eq!(gateway.expect_sql("UPDATE inv SET stock = stock + price * 2 WHERE id > 0").affected_rows, 2);
    assert_eq!(gateway.scalar_integer("SELECT stock FROM inv WHERE id = 1"), 13);
    assert_eq!(gateway.scalar_integer("SELECT stock FROM inv WHERE id = 2"), 11);

    // 列引用与字符串拼接函数：`SET name = (CONCAT(name, 'x'))`。
    // 括号是必需的：钉住的 data-text-sql 解析器把 SET 右值的裸函数调用报成
    // "Missing value in assignment"，括号形式才产出 Expression 节点。
    assert_eq!(gateway.expect_sql("UPDATE inv SET name = (CONCAT(name, 'x')) WHERE id = 1").affected_rows, 1);
    assert_eq!(gateway.scalar_text("SELECT name FROM inv WHERE id = 1"), "ax");

    // 取负一元表达式。
    assert_eq!(gateway.expect_sql("UPDATE inv SET stock = -stock WHERE id = 2").affected_rows, 1);
    assert_eq!(gateway.scalar_integer("SELECT stock FROM inv WHERE id = 2"), -11);
}

/// 多条赋值共享同时赋值语义：右值全部基于改写前的行，`SET a = b, b = a` 完成交换。
#[test]
fn update_set_multiple_assignments_use_pre_image() {
    let gateway = Gateway::new("update-swap");
    gateway.expect_sql("CREATE TABLE pair (id INT NOT NULL PRIMARY KEY, a INT, b INT)");
    gateway.expect_sql("INSERT INTO pair (id, a, b) VALUES (1, 3, 7)");
    assert_eq!(gateway.expect_sql("UPDATE pair SET a = b, b = a WHERE id = 1").affected_rows, 1);
    assert_eq!(gateway.scalar_integer("SELECT a FROM pair WHERE id = 1"), 7);
    assert_eq!(gateway.scalar_integer("SELECT b FROM pair WHERE id = 1"), 3);
}

/// 表达式求值结果仍走同一类型守卫：把文本列写进整数列报 `TypeMismatch`，
/// 纯字面量的计划期校验保持不变。
#[test]
fn update_set_expression_keeps_type_guards() {
    let gateway = Gateway::new("update-guard");
    gateway.expect_sql("CREATE TABLE guard (id INT NOT NULL PRIMARY KEY, n INT, t TEXT)");
    gateway.expect_sql("INSERT INTO guard (id, n, t) VALUES (1, 2, 'x')");
    let error = gateway.sql("UPDATE guard SET n = t WHERE id = 1").expect_err("string must not be written to an INT column");
    assert_eq!(error.kind, database_query::SqlErrorKind::TypeMismatch);
    let error = gateway.sql("UPDATE guard SET n = 'abc' WHERE id = 1").expect_err("non-numeric literal must fail at plan time");
    assert_eq!(error.kind, database_query::SqlErrorKind::TypeMismatch);
    // NOT NULL 列写入 NULL 仍被拒绝：纯字面量在计划期报，表达式在求值后报同一守卫。
    gateway.expect_sql("CREATE TABLE notnull (id INT NOT NULL PRIMARY KEY, n INT NOT NULL)");
    gateway.expect_sql("INSERT INTO notnull (id, n) VALUES (1, 1)");
    let error = gateway.sql("UPDATE notnull SET n = NULL WHERE id = 1").expect_err("NULL into NOT NULL must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::NotNullViolation);
    let error = gateway.sql("UPDATE notnull SET n = n * 0 / 0 WHERE id = 1").expect_err("MySQL 0/0 is NULL, the NOT NULL guard must fire");
    assert_eq!(error.kind, database_query::SqlErrorKind::NotNullViolation);
    // 谓词不命中任何行：0 影响行，不报错。
    assert_eq!(gateway.expect_sql("UPDATE notnull SET n = n + 1 WHERE id = 999").affected_rows, 0);
}

/// `INSERT` 的 VALUES 仍保持 literal-only 合同（本任务不放开 INSERT）。
#[test]
fn insert_values_still_reject_row_expressions() {
    let gateway = Gateway::new("insert-literal");
    gateway.expect_sql("CREATE TABLE frozen (id INT NOT NULL PRIMARY KEY, n INT)");
    let error = gateway.sql("INSERT INTO frozen (id, n) VALUES (1, 2 + 1)").expect_err("INSERT VALUES must stay literal-only");
    assert!(error.message.contains("literal"), "unexpected error: {}", error.message);
}

/// 聚合遇主键等值必须退回扫描：单点读路径不承载聚合求值。
/// 历史缺陷：`SELECT COUNT(*) ... WHERE id = 等值` 走点读后聚合报 internal error。
#[test]
fn aggregate_over_primary_key_equality_falls_back_to_scan() {
    let gateway = gateway_with_hundred_rows("agg-pk-eq");
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id = 42"), 1);
    assert_eq!(gateway.scalar_integer("SELECT MAX(v) FROM nums WHERE id = 7"), 7);
    // 等值不命中任何行：聚合仍按扫描语义返回空集口径（COUNT=0）。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id = 999"), 0);
}

/// `IN (值列表)` 必须命中列表里的每一个值，且大列表不能因为逐值展开的 `OR`
/// 链过深而耗尽线程栈。历史缺陷：折叠成等值集合时丢掉了第一个字面量。
#[test]
fn in_list_matches_every_listed_value() {
    let gateway = gateway_with_hundred_rows("in-list");
    let ids = gateway.expect_sql("SELECT id FROM nums WHERE id IN (1, 3) ORDER BY id");
    assert_eq!(ids.rows, vec![vec![SqlValue::Integer(1)], vec![SqlValue::Integer(3)]]);
    // 含 NULL 的列表：命中返回 TRUE，未命中返回 NULL（COUNT 不计），`NOT IN` 同口径。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id IN (1, NULL)"), 1);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM nums WHERE id NOT IN (1, NULL)"), 0);
    // 500 个值：覆盖旧实现在这个规模上栈溢出的形态，结果按表内 100 行计。
    let values: Vec<String> = (1..=500).map(|id| id.to_string()).collect();
    let result = gateway.expect_sql(&format!("SELECT COUNT(*) FROM nums WHERE id IN ({})", values.join(", ")));
    assert_eq!(result.rows[0][0], SqlValue::Integer(100));
}

/// 等值连接只产出匹配组合，基表谓词剪枝不改变结果集。
#[test]
fn equality_join_matches_every_paired_row() {
    let gateway = Gateway::new("join-equality");
    gateway.expect_sql("CREATE TABLE lside (id INT NOT NULL PRIMARY KEY, grp INT)");
    gateway.expect_sql("CREATE TABLE rside (id INT NOT NULL PRIMARY KEY, name TEXT)");
    gateway.expect_sql("INSERT INTO lside (id, grp) VALUES (1, 0), (2, 0), (3, 1)");
    gateway.expect_sql("INSERT INTO rside (id, name) VALUES (1, 'a'), (2, 'b'), (4, 'd')");
    // 基表谓词 `grp = 0` 先剪枝，连接只应产出 2 条匹配。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM lside l JOIN rside r ON r.id = l.id WHERE l.grp = 0"), 2);
    let names = gateway.expect_sql("SELECT r.name FROM lside l JOIN rside r ON r.id = l.id ORDER BY r.id");
    assert_eq!(names.rows, vec![vec![SqlValue::String("a".into())], vec![SqlValue::String("b".into())]]);
    // LEFT JOIN：未匹配的左侧行保留，共 3 行。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM lside l LEFT JOIN rside r ON r.id = l.id"), 3);
}

/// 绑定层形状冒烟：确认 bind 产出的 Update 语句进入计划器而不是被更早的层拒绝。
#[test]
fn update_statement_binds_to_update_variant() {
    let gateway = Gateway::new("bind-shape");
    gateway.expect_sql("CREATE TABLE shape (id INT NOT NULL PRIMARY KEY, n INT)");
    let catalog = gateway.registry.catalog();
    let bound =
        bind_sql_catalog("UPDATE shape SET n = n + 1 WHERE id = 1", &catalog, catalog.schema_version(), SqlDialect::Mysql).expect("bind");
    assert!(matches!(bound, SqlCatalogBoundStatement::Update(_)));
}

/// 同宽度的四位数主键范围会下推成 KV 字节边界，结果仍由剩余谓词复核。
#[test]
fn four_digit_primary_key_range_pushes_bounds_without_losing_rows() {
    let gateway = Gateway::new("pk-pushdown");
    gateway.expect_sql("CREATE TABLE wide (id INT NOT NULL PRIMARY KEY, v INT)");
    let values: Vec<String> = (1000..=1199).map(|id| format!("({id}, {id})")).collect();
    gateway.expect_sql(&format!("INSERT INTO wide (id, v) VALUES {}", values.join(", ")));
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM wide WHERE id > 1100 AND id <= 1120"), 20);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM wide WHERE id >= 1100 AND id < 1120"), 20);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM wide WHERE id BETWEEN 1000 AND 1099"), 100);
    // 位数不同的边界不下推字节边界，数值语义必须保持。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM wide WHERE id > 999 AND id <= 1000"), 1);
}

/// `ORDER BY <主键> LIMIT n` 的快速路径只有在 key 数值严格递增时才启用；
/// 混合位数的 rowid 会回退到排序路径，结果仍是数值序。
#[test]
fn mixed_width_primary_key_order_by_keeps_numeric_order() {
    let gateway = Gateway::new("pk-order");
    gateway.expect_sql("CREATE TABLE seq (id INT NOT NULL PRIMARY KEY, v INT)");
    gateway.expect_sql("INSERT INTO seq (id, v) VALUES (9, 9), (10, 10), (99, 99), (100, 100), (1000, 1000)");
    let result = gateway.expect_sql("SELECT id FROM seq ORDER BY id LIMIT 3");
    assert_eq!(result.rows, vec![vec![SqlValue::Integer(9)], vec![SqlValue::Integer(10)], vec![SqlValue::Integer(99)],]);
}

/// 列级 `UNIQUE` 与表级 `UNIQUE KEY` 一样生效；MySQL 的
/// `ON DUPLICATE KEY UPDATE` 覆盖主键与次要唯一键两种冲突。
#[test]
fn inline_unique_and_on_duplicate_key_update_follow_mysql_semantics() {
    let gateway = Gateway::new("upsert");
    gateway.expect_sql("CREATE TABLE ups (id INT NOT NULL PRIMARY KEY, uk INT UNIQUE, v INT, pad VARCHAR(8))");
    gateway.expect_sql("INSERT INTO ups (id, uk, v, pad) VALUES (1, 10, 1, 'a')");

    // 主键冲突：`VALUES(col)` 取提议行的值，MySQL 对改写报 2 行。
    assert_eq!(
        gateway
            .expect_sql("INSERT INTO ups (id, uk, v, pad) VALUES (1, 10, 2, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v), pad = VALUES(pad)")
            .affected_rows,
        2
    );
    assert_eq!(gateway.scalar_integer("SELECT v FROM ups WHERE id = 1"), 2);
    assert_eq!(gateway.scalar_text("SELECT pad FROM ups WHERE id = 1"), "b");

    // 次要唯一键冲突：改写实际冲突的那一行，主键保持不变，也不插入新行。
    assert_eq!(
        gateway.expect_sql("INSERT INTO ups (id, uk, v, pad) VALUES (2, 10, 3, 'c') ON DUPLICATE KEY UPDATE v = VALUES(v)").affected_rows,
        2
    );
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM ups"), 1);
    assert_eq!(gateway.scalar_integer("SELECT v FROM ups WHERE id = 1"), 3);

    // 没有冲突策略时，列级 UNIQUE 仍按 1062 拒绝。
    let error = gateway.sql("INSERT INTO ups (id, uk, v, pad) VALUES (3, 10, 4, 'd')").expect_err("duplicate unique must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::DuplicateKey);
}

/// `INSERT ... SELECT` 把源查询的每一行写入目标表。
#[test]
fn insert_select_writes_every_source_row() {
    let gateway = Gateway::new("insert-select");
    gateway.expect_sql("CREATE TABLE src (id INT NOT NULL PRIMARY KEY, v INT)");
    gateway.expect_sql("INSERT INTO src (id, v) VALUES (1, 10), (2, 20), (3, 30)");
    gateway.expect_sql("CREATE TABLE dst (id INT NOT NULL PRIMARY KEY, v INT)");
    assert_eq!(gateway.expect_sql("INSERT INTO dst (id, v) SELECT id, v FROM src WHERE id > 1").affected_rows, 2);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM dst"), 2);
    assert_eq!(gateway.scalar_integer("SELECT SUM(v) FROM dst"), 50);
}

/// `DATE_TRUNC` 是 PostgreSQL 拼写，MySQL 方言报未知函数。
#[test]
fn mysql_dialect_rejects_date_trunc() {
    let gateway = Gateway::new("date-trunc");
    let error = gateway.sql("SELECT DATE_TRUNC('day', '2026-11-18 13:45:30')").expect_err("MySQL has no DATE_TRUNC");
    assert_eq!(error.kind, database_query::SqlErrorKind::UnknownFunction);
}

/// `CHECK` 约束在写入前对最终行值求值：只有 FALSE 违反，`NULL` 通过（SQL 三值
/// 逻辑，与 PostgreSQL 一致），违反按 `CheckViolation` 上报。
#[test]
fn check_constraint_rejects_false_and_allows_null() {
    let gateway = Gateway::new("check");
    gateway.expect_sql("CREATE TABLE guarded (id INT NOT NULL PRIMARY KEY, qty INT CHECK (qty >= 0), note TEXT)");
    gateway.expect_sql("INSERT INTO guarded (id, qty, note) VALUES (1, 5, 'ok')");
    // `NULL` 不违反 `CHECK`。
    gateway.expect_sql("INSERT INTO guarded (id, qty, note) VALUES (2, NULL, 'null-ok')");
    let error = gateway.sql("INSERT INTO guarded (id, qty, note) VALUES (3, -1, 'bad')").expect_err("negative qty must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::CheckViolation);
    // UPDATE 走同一门禁。
    let error = gateway.sql("UPDATE guarded SET qty = -7 WHERE id = 1").expect_err("update into negative must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::CheckViolation);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM guarded"), 2);
    // 多行 INSERT 原子路径：违反行在提交边界前拦截，整批不发布。
    let error = gateway.sql("INSERT INTO guarded (id, qty) VALUES (4, 1), (5, -2)").expect_err("batch must reject the violating row");
    assert_eq!(error.kind, database_query::SqlErrorKind::CheckViolation);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM guarded"), 2);
}

/// `FOREIGN KEY` 写时强制孤儿引用：非空子列按父表主键点查，未命中按
/// `ForeignKeyViolation` 上报；`NULL` 引用视为满足。
#[test]
fn foreign_key_rejects_orphan_references() {
    let gateway = Gateway::new("foreign-key");
    gateway.expect_sql("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, name TEXT)");
    gateway.expect_sql("CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT REFERENCES parent (id))");
    gateway.expect_sql("INSERT INTO parent (id, name) VALUES (1, 'p1')");
    gateway.expect_sql("INSERT INTO child (id, parent_id) VALUES (10, 1)");
    gateway.expect_sql("INSERT INTO child (id, parent_id) VALUES (11, NULL)");
    let error = gateway.sql("INSERT INTO child (id, parent_id) VALUES (12, 99)").expect_err("orphan reference must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::ForeignKeyViolation);
    // UPDATE 走同一门禁。
    let error = gateway.sql("UPDATE child SET parent_id = 98 WHERE id = 10").expect_err("update into orphan must fail");
    assert_eq!(error.kind, database_query::SqlErrorKind::ForeignKeyViolation);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM child"), 2);
    // MySQL 方言的 `||` 是逻辑 OR，网关不把它当拼接，按源文本显式拒绝。
    let error = gateway.sql("SELECT name || 'x' FROM parent").expect_err("MySQL dialect must reject `||`");
    assert_eq!(error.kind, database_query::SqlErrorKind::Syntax);
    // 表级 `FOREIGN KEY` 与内联 `REFERENCES` 同口径。
    gateway.expect_sql("CREATE TABLE child2 (id INT NOT NULL PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES parent (id))");
    let error = gateway.sql("INSERT INTO child2 (id, pid) VALUES (20, 77)").expect_err("table-level foreign key must be enforced");
    assert_eq!(error.kind, database_query::SqlErrorKind::ForeignKeyViolation);
}

/// 普通（非唯一）二级索引：同一列值可以有多行，条目不能互相覆盖。
///
/// 断言两件事：等值谓词真的走索引探针；插入、更新、删除都同步维护条目。
#[test]
fn non_unique_secondary_index_keeps_duplicates_and_tracks_writes() {
    let gateway = Gateway::new("secondary-index");
    gateway.expect_sql("CREATE TABLE events (id INT NOT NULL PRIMARY KEY, user_id INT NOT NULL, note VARCHAR(16))");
    gateway.expect_sql("CREATE INDEX idx_events_user ON events (user_id)");
    gateway.expect_sql("INSERT INTO events (id, user_id, note) VALUES (1, 7, 'a'), (2, 7, 'b'), (3, 9, 'c')");

    // 等值条件必须规划成索引探针，而不是整表扫描。
    let catalog = gateway.registry.catalog();
    let bound = bind_sql_catalog("SELECT id FROM events WHERE user_id = 7", &catalog, catalog.schema_version(), SqlDialect::Mysql)
        .expect("bind indexed equality");
    let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Mysql).expect("plan indexed equality");
    let probe = match plan {
        database_query::SqlExecutionPlan::Scan(scan) => scan.index,
        other => panic!("expected a scan plan, got {other:?}"),
    };
    assert!(probe.is_some(), "equality on an indexed column must probe the index");

    // 同一列值的三行都要被索引覆盖。
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 7"), 2);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 9"), 1);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 8"), 0);

    // 删除后条目同步摘除。
    gateway.expect_sql("DELETE FROM events WHERE id = 1");
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 7"), 1);

    // 改写索引列：旧值条目消失、新值可查。
    gateway.expect_sql("UPDATE events SET user_id = 10 WHERE id = 2");
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 7"), 0);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM events WHERE user_id = 10"), 1);
}

/// UUID 主键按 16 字节二进制 rowid 存储，写读都不再经过 36 字符文本。
///
/// 断言三件事：点读 rowid 恰好 16 字节；任意大小写/连字符写法都能命中同一行；
/// 读回的文本是规范小写连字符形式。
#[test]
fn uuid_primary_key_stores_sixteen_octets_and_matches_any_spelling() {
    let gateway = Gateway::new("uuid-primary-key");
    gateway.expect_sql("CREATE TABLE users (id UUID NOT NULL PRIMARY KEY, nickname VARCHAR(32) NOT NULL)");
    gateway.expect_sql(
        "INSERT INTO users (id, nickname) VALUES ('3F2504E0-4F89-11D3-9A0C-0305E82C3301', 'alice'), ('00000000-0000-0000-0000-000000000002', 'bob')",
    );

    // 点读计划里的 rowid 必须是 16 字节，不是 36 字符文本。
    let catalog = gateway.registry.catalog();
    let bound = bind_sql_catalog(
        "SELECT nickname FROM users WHERE id = '3F2504E0-4F89-11D3-9A0C-0305E82C3301'",
        &catalog,
        catalog.schema_version(),
        SqlDialect::Mysql,
    )
    .expect("bind uuid point read");
    let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Mysql).expect("plan uuid point read");
    let rowid = match &plan {
        database_query::SqlExecutionPlan::Read(read) => read.rowid.clone(),
        other => panic!("expected a point read, got {other:?}"),
    };
    assert_eq!(rowid.len(), 16, "uuid primary key rowid must be the 16 canonical octets");

    // 大写连字符、小写无连字符、纯十六进制三种写法命中同一行。
    assert_eq!(gateway.scalar_text("SELECT nickname FROM users WHERE id = '3F2504E0-4F89-11D3-9A0C-0305E82C3301'"), "alice");
    assert_eq!(gateway.scalar_text("SELECT nickname FROM users WHERE id = '3f2504e0-4f89-11d3-9a0c-0305e82c3301'"), "alice");
    assert_eq!(gateway.scalar_text("SELECT nickname FROM users WHERE id = '3f2504e04f8911d39a0c0305e82c3301'"), "alice");
    assert_eq!(gateway.scalar_text("SELECT id FROM users WHERE nickname = 'bob'"), "00000000-0000-0000-0000-000000000002");

    // 主键冲突按值而不是写法判定。
    assert!(gateway.sql("INSERT INTO users (id, nickname) VALUES ('3f2504e04f8911d39a0c0305e82c3301', 'dup')").is_err());

    // 非法 uuid 文本被拒绝，而不是写成文本落库。
    assert!(gateway.sql("INSERT INTO users (id, nickname) VALUES ('not-a-uuid', 'x')").is_err());
}

/// uuid 列上的二级索引：条目载荷与行存储同为 16 字节，等值探测能命中。
#[test]
fn uuid_secondary_index_probes_sixteen_octet_payloads() {
    let gateway = Gateway::new("uuid-secondary-index");
    gateway.expect_sql("CREATE TABLE sessions (id INT NOT NULL PRIMARY KEY, user_id UUID NOT NULL)");
    gateway.expect_sql("CREATE INDEX idx_sessions_user ON sessions (user_id)");
    gateway.expect_sql(
        "INSERT INTO sessions (id, user_id) VALUES (1, '3F2504E0-4F89-11D3-9A0C-0305E82C3301'), (2, '3F2504E0-4F89-11D3-9A0C-0305E82C3301'), (3, '00000000-0000-0000-0000-000000000002')",
    );

    let catalog = gateway.registry.catalog();
    let bound = bind_sql_catalog(
        "SELECT id FROM sessions WHERE user_id = '3f2504e0-4f89-11d3-9a0c-0305e82c3301'",
        &catalog,
        catalog.schema_version(),
        SqlDialect::Mysql,
    )
    .expect("bind indexed uuid equality");
    let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Mysql).expect("plan indexed uuid equality");
    let probe = match plan {
        database_query::SqlExecutionPlan::Scan(scan) => scan.index,
        other => panic!("expected a scan plan, got {other:?}"),
    };
    assert!(probe.is_some(), "equality on an indexed uuid column must probe the index");

    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM sessions WHERE user_id = '3F2504E0-4F89-11D3-9A0C-0305E82C3301'"), 2);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM sessions WHERE user_id = '00000000-0000-0000-0000-000000000002'"), 1);
    assert_eq!(gateway.scalar_integer("SELECT COUNT(*) FROM sessions WHERE user_id = '00000000-0000-0000-0000-000000000003'"), 0);
}

/// 经典计数下钻：`WITH RECURSIVE r AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 10)`
/// 必须收敛到 10 行，consumer 的聚合与排序都读全量结果。
#[test]
fn recursive_cte_union_all_counts_down_to_fixpoint() {
    let gateway = Gateway::new("recursive-count");
    assert_eq!(
        gateway.scalar_integer("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT COUNT(*) FROM r"),
        10
    );
    assert_eq!(
        gateway.scalar_integer("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT SUM(n) FROM r"),
        55
    );
    // 全量结果按插入序（BFS 语义）可投影、可过滤。
    let result = gateway.expect_sql("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT n FROM r WHERE n > 8");
    assert_eq!(result.rows, vec![vec![SqlValue::Integer(9)], vec![SqlValue::Integer(10)]]);
}

/// `UNION`（去重）与 `UNION ALL`（全收）的分歧：step 生成 {1,2} 环时，
/// 去重版只收敛到 2 行，全收版在轮次上限处报错。
#[test]
fn recursive_cte_union_deduplicates_and_union_all_terminates_only_by_bound() {
    let gateway = Gateway::new("recursive-dedup");
    // 1↔2 环：UNION 去重后第二轮就没有新行。
    assert_eq!(
        gateway.scalar_integer("WITH RECURSIVE r AS (SELECT 1 AS n UNION SELECT 3 - n FROM r WHERE n < 3) SELECT COUNT(*) FROM r"),
        2
    );
    // 同一个环改成 UNION ALL：行数每轮 +1，永不收敛，撞上轮次上限。
    let error = gateway
        .sql("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT 3 - n FROM r WHERE n < 3) SELECT COUNT(*) FROM r")
        .expect_err("a growing UNION ALL must not run forever");
    assert!(error.message.contains("exceeded 10000 rounds without reaching a fixpoint"), "unexpected error: {}", error.message);
}

/// 树遍历：seed 从真实表取根节点，step 按 parent 关系下钻；显式列清单改名生效。
#[test]
fn recursive_cte_walks_a_tree_from_a_real_table_seed() {
    let gateway = Gateway::new("recursive-tree");
    gateway.expect_sql("CREATE TABLE nodes (id INT NOT NULL PRIMARY KEY, parent INT, label TEXT)");
    gateway.expect_sql("INSERT INTO nodes (id, parent, label) VALUES (1, NULL, 'root'), (2, 1, 'a'), (3, 1, 'b'), (4, 2, 'aa'), (5, 4, 'aaa')");
    // 从 root 出发的两种形态都要验证：`IN (SELECT n FROM subtree)` 被子查询合同
    // 拒绝，语义等价的 JOIN 形态才是网关支持的树遍历写法。
    let error = gateway.sql(
        "WITH RECURSIVE subtree (n) AS (SELECT id FROM nodes WHERE parent IS NULL UNION ALL SELECT id FROM nodes WHERE parent IN (SELECT n FROM subtree)) SELECT n FROM subtree ORDER BY n",
    )
    .expect_err("a subquery cannot scan the fixed point");
    assert!(error.message.contains("recursive CTE") || error.message.contains("materializes"), "unexpected error: {}", error.message);
    let descendants = gateway.expect_sql(
        "WITH RECURSIVE subtree (id, label, depth) AS (SELECT id, label, 0 FROM nodes WHERE parent IS NULL UNION ALL SELECT n.id, n.label, subtree.depth + 1 FROM nodes n JOIN subtree ON n.parent = subtree.id) SELECT COUNT(*) FROM subtree",
    );
    assert_eq!(descendants.rows[0][0], SqlValue::Integer(5));
    let deepest = gateway
        .scalar_integer("WITH RECURSIVE subtree (id, label, depth) AS (SELECT id, label, 0 FROM nodes WHERE parent IS NULL UNION ALL SELECT n.id, n.label, subtree.depth + 1 FROM nodes n JOIN subtree ON n.parent = subtree.id) SELECT MAX(depth) FROM subtree");
    assert_eq!(deepest, 3);
}

/// 两条递归 CTE 按书写顺序级联：后一条的 seed 能扫到前一条的全量结果。
#[test]
fn two_recursive_ctes_chain_in_source_order() {
    let gateway = Gateway::new("recursive-chain");
    let result = gateway.expect_sql(
        "WITH RECURSIVE a (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a WHERE n < 3), b (m) AS (SELECT n * 10 FROM a UNION ALL SELECT m + 1 FROM b WHERE m < 12) SELECT COUNT(*) FROM b",
    );
    // b 的 seed 扫 a 的全量 {1,2,3} → {10,20,30}；step 只把 10 推到 11、12，
    // 20/30 越过界不产出。全量 {10,11,12,20,30} 共 5 行。
    assert_eq!(result.rows[0][0], SqlValue::Integer(5));
    let values = gateway.expect_sql(
        "WITH RECURSIVE a (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a WHERE n < 3), b (m) AS (SELECT n * 10 FROM a UNION ALL SELECT m + 1 FROM b WHERE m < 12) SELECT m FROM b ORDER BY m",
    );
    assert_eq!(
        values.rows,
        vec![
            vec![SqlValue::Integer(10)],
            vec![SqlValue::Integer(11)],
            vec![SqlValue::Integer(12)],
            vec![SqlValue::Integer(20)],
            vec![SqlValue::Integer(30)],
        ]
    );
}

/// 范围控制：同一查询两次引用同一条递归 CTE（不动点自连接）在绑定期拒绝，
/// 子查询引用同样拒绝，绝不静默地共享一份行槽。
#[test]
fn recursive_cte_rejects_second_reference_and_subquery_scan() {
    let gateway = Gateway::new("recursive-scope");
    let error = gateway
        .sql("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT * FROM r a, r b")
        .expect_err("a self join of the fixed point must be refused");
    assert!(error.message.contains("more than once"), "unexpected error: {}", error.message);
    gateway.expect_sql("CREATE TABLE probe (id INT NOT NULL PRIMARY KEY)");
    gateway.expect_sql("INSERT INTO probe (id) VALUES (1), (2)");
    let error = gateway
        .sql("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT id FROM probe WHERE id IN (SELECT n FROM r)")
        .expect_err("a subquery cannot scan the fixed point");
    assert!(error.message.contains("recursive CTE") || error.message.contains("materializes"), "unexpected error: {}", error.message);
}

/// 工作集是语句局部的：递归查询结束后，合成 namespace 不再出现在任何后续
/// 扫描里；重复执行同一递归语句得到相同结果。
#[test]
fn recursive_cte_working_set_leaves_no_trace_between_statements() {
    let gateway = Gateway::new("recursive-clean");
    let first = gateway.scalar_integer("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT SUM(n) FROM r");
    let second = gateway.scalar_integer("WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT SUM(n) FROM r");
    assert_eq!(first, 15);
    assert_eq!(second, 15);
    // 普通表扫描不受工作集影响。
    gateway.expect_sql("CREATE TABLE plain (id INT NOT NULL PRIMARY KEY)");
    gateway.expect_sql("INSERT INTO plain (id) VALUES (7)");
    assert_eq!(gateway.scalar_integer("SELECT id FROM plain"), 7);
}

/// `WITH RECURSIVE` 形态的边界：没有 UNION 递归操作数的定义被拒绝；非递归
/// `WITH` 的集合体保持原拒绝。
#[test]
fn recursive_shape_guards_stay_closed() {
    let gateway = Gateway::new("recursive-guards");
    let error = gateway
        .sql("WITH RECURSIVE r AS (SELECT 1 AS n) SELECT * FROM r")
        .expect_err("a recursive definition needs a UNION operand");
    assert!(error.message.contains("UNION"), "unexpected error: {}", error.message);
    let error = gateway.sql("WITH r AS (SELECT 1 AS n UNION SELECT 2) SELECT * FROM r").expect_err("plain WITH keeps rejecting set bodies");
    assert!(error.message.contains("set-operation"), "unexpected error: {}", error.message);
}
