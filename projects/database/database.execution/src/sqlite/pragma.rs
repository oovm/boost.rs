//! SQLite `PRAGMA` 机器:读形 PRAGMA 的结果行与镜像完整性检查。
//!
//! 本模块拥有 `integrity_check` 的镜像遍历与 freelist 校验、`table_info` /
//! `table_xinfo` / `index_list` / `index_info` / `foreign_key_list` 的目录
//! 行集推导,以及 describe 路径复用的表 / 视图定位与声明类型归一。
//! 连接级状态臂(`foreign_keys` / `recursive_triggers` / `user_version` /
//! `application_id` / `database_list`)留在门面分发点;本模块只接收显式
//! 借用,不得反向依赖任何产品门面。

use std::collections::BTreeSet;

use database_formats::sqlite::{read_freelist_count, read_freelist_trunk, validate_database};
use database_query::sqlite::{SqlError, catalog::*, value::*};
use database_storage::sqlite::{SchemaObjectKind, TableLimits, read_index_table_with, read_table};

use super::{
    probe::autoindex_definitions,
    select::ExecContext,
    stmt::{ExecResult, StmtContext},
};

/// `PRAGMA integrity_check`: walks the header, the schema, every table and
/// index b-tree, and the freelist chain over the current image. A healthy
/// image reports one `ok` row; a structural problem is reported as a row of
/// text, matching SQLite's non-failing result shape.
///
/// The check reads pages, it does not trust the in-memory catalog or the
/// writer's bookkeeping, so a decoder failure surfaces here instead of
/// being answered with a hard-coded `ok`. Page-reachability accounting
/// (detecting a page shared by two b-trees) is outside this walk because
/// the reader API validates each tree in isolation.
pub fn integrity_check(ctx: &mut StmtContext<'_>, argument: &str) -> ExecResult {
    // 检查走的是镜像页,先把推迟的索引重建落下,否则会把旧索引页判成问题。
    if let Err(error) = ctx.flush_pending_materialization("integrity-check") {
        return ExecResult {
            statement: "pragma",
            columns: vec!["integrity_check".to_string()],
            rows: vec![vec![Cell::Text(error.to_string())]],
            affected: 0,
        };
    }
    let mut problems = Vec::new();
    let pages = ctx.writer.pages();
    let limits = TableLimits::query();
    match validate_database(pages) {
        Ok(_) => {
            let objects = match ctx.writer.schema_objects(limits) {
                Ok(objects) => objects,
                Err(error) => {
                    problems.push(format!("schema: {error}"));
                    Vec::new()
                }
            };
            for object in &objects {
                let Some(root) = object.root_page.filter(|root| *root != 0)
                else {
                    continue;
                };
                let outcome = match object.kind {
                    SchemaObjectKind::Index => {
                        let collations = ctx
                            .catalog
                            .indexes
                            .get(&object.name.to_ascii_lowercase())
                            .map(|index| index.collations.clone())
                            .unwrap_or_default();
                        read_index_table_with(pages, root, limits, &collations).map(|_| ())
                    }
                    SchemaObjectKind::Table if table_is_without_rowid(ctx.catalog, &object.name) => {
                        let collations = match ctx.catalog.tables.iter().find(|def| def.name.eq_ignore_ascii_case(&object.name)) {
                            Some(def) => {
                                let layout = def.without_rowid_layout();
                                layout[..def.primary_key_positions().map(|keys| keys.len()).unwrap_or(0)]
                                    .iter()
                                    .map(|position| def.columns[*position].collate.unwrap_or_default())
                                    .collect()
                            }
                            None => Vec::new(),
                        };
                        read_index_table_with(pages, root, limits, &collations).map(|_| ())
                    }
                    SchemaObjectKind::Table => read_table(pages, root, limits).map(|_| ()),
                    SchemaObjectKind::View | SchemaObjectKind::Trigger => Ok(()),
                };
                if let Err(error) = outcome {
                    let kind = match object.kind {
                        SchemaObjectKind::Table => "table",
                        SchemaObjectKind::Index => "index",
                        SchemaObjectKind::View => "view",
                        SchemaObjectKind::Trigger => "trigger",
                    };
                    problems.push(format!("{kind} `{}`: {error}", object.name));
                }
            }
            collect_freelist_problems(ctx.writer.page_size() as usize, pages, &mut problems);
        }
        Err(error) => problems.push(error.to_string()),
    }
    let limit = argument.trim().parse::<usize>().ok().filter(|limit| *limit > 0).unwrap_or(100);
    let rows = if problems.is_empty() {
        vec![vec![Cell::Text("ok".to_string())]]
    }
    else {
        problems.iter().take(limit).map(|problem| vec![Cell::Text(problem.clone())]).collect()
    };
    ExecResult { statement: "pragma", columns: vec!["integrity_check".to_string()], rows, affected: 0 }
}

/// Matches a schema table name against the catalog to know whether its
/// root is a table b-tree or a primary-key index b-tree.
fn table_is_without_rowid(catalog: &SqliteCatalog, name: &str) -> bool {
    catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(name) && def.without_rowid)
}

/// Replays the freelist chain and checks that the trunk pages and leaves
/// account for exactly the count declared in the file header.
fn collect_freelist_problems(page_size: usize, pages: &[u8], problems: &mut Vec<String>) {
    if page_size == 0 || pages.len() < page_size {
        return;
    }
    let page_count = pages.len() / page_size;
    let declared = read_freelist_count(pages) as usize;
    let mut trunk = read_freelist_trunk(pages);
    let mut seen = BTreeSet::new();
    let mut total = 0usize;
    while trunk != 0 {
        let index = trunk as usize;
        if index == 0 || index > page_count {
            problems.push(format!("freelist trunk page {trunk} is out of range"));
            return;
        }
        if !seen.insert(trunk) {
            problems.push(format!("freelist trunk page {trunk} is repeated"));
            return;
        }
        total += 1;
        let start = (index - 1) * page_size;
        let page = &pages[start..start + page_size];
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        let leaves = u32::from_be_bytes([page[4], page[5], page[6], page[7]]) as usize;
        let capacity = page_size / 4 - 2;
        if leaves > capacity {
            problems.push(format!("freelist trunk page {trunk} declares {leaves} leaves beyond capacity {capacity}"));
            return;
        }
        for leaf_index in 0..leaves {
            let offset = 8 + leaf_index * 4;
            let leaf = u32::from_be_bytes([page[offset], page[offset + 1], page[offset + 2], page[offset + 3]]) as usize;
            if leaf == 0 || leaf > page_count {
                problems.push(format!("freelist leaf page {leaf} on trunk {trunk} is out of range"));
                continue;
            }
            if !seen.insert(leaf as u32) {
                problems.push(format!("freelist leaf page {leaf} is repeated"));
                continue;
            }
            total += 1;
        }
        trunk = next;
    }
    if total != declared {
        problems.push(format!("freelist declares {declared} pages but the chain holds {total}"));
    }
}

/// Finds the table one `PRAGMA` refers to. An unqualified name searches the
/// temp schema first, then `main`, matching SQLite's connection search order.
pub fn table_def<'a>(catalog: &'a SqliteCatalog, schema: Option<&str>, name: &str) -> Option<&'a TableDef> {
    let key = name.to_ascii_lowercase();
    let wants_temp = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("temp"));
    let wants_main = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("main"));
    if wants_temp {
        if let Some(position) = catalog.temp_lookup.get(&key) {
            return Some(&catalog.temp_tables[*position].def);
        }
    }
    if wants_main {
        if let Some(position) = catalog.table_lookup.get(&key) {
            return Some(&catalog.tables[*position]);
        }
    }
    None
}

/// Finds the view one `PRAGMA` refers to, preferring the temp schema.
pub fn view_def<'a>(catalog: &'a SqliteCatalog, schema: Option<&str>, name: &str) -> Option<&'a ViewDef> {
    let key = name.to_ascii_lowercase();
    let wants_temp = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("temp"));
    let wants_main = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("main"));
    if wants_temp {
        if let Some(view) = catalog.temp_views.get(&key) {
            return Some(&view.def);
        }
    }
    if wants_main {
        if let Some(view) = catalog.views.get(&key) {
            return Some(view);
        }
    }
    None
}

/// The declared type text `PRAGMA table_info` reports: SQLite normalises the
/// four storage-class keywords to upper case and echoes everything else as it
/// was written.
fn declared_type(declared: &str) -> String {
    let text = declared.trim();
    match text.to_ascii_lowercase().as_str() {
        "integer" | "text" | "real" | "blob" => text.to_ascii_uppercase(),
        _ => text.to_string(),
    }
}

/// `PRAGMA table_info(t)` / `PRAGMA table_xinfo(t)`.
pub fn table_info(exec: &ExecContext<'_>, schema: Option<&str>, table: &str, extended: bool) -> Result<ExecResult, SqlError> {
    let mut columns =
        vec!["cid".to_string(), "name".to_string(), "type".to_string(), "notnull".to_string(), "dflt_value".to_string(), "pk".to_string()];
    if extended {
        columns.push("hidden".to_string());
    }
    let mut rows = Vec::new();
    if let Some(def) = table_def(exec.catalog, schema, table) {
        let key_positions = def.primary_key_positions().unwrap_or_default();
        for (position, column) in def.columns.iter().enumerate() {
            let pk = key_positions.iter().position(|candidate| *candidate == position).map(|slot| slot as i64 + 1).unwrap_or(0);
            let mut row = vec![
                Cell::Integer(position as i64),
                Cell::Text(column.name.clone()),
                Cell::Text(declared_type(&column.declared_type)),
                Cell::Integer(i64::from(column.not_null)),
                column.default_text.clone().map(Cell::Text).unwrap_or(Cell::Null),
                Cell::Integer(pk),
            ];
            if extended {
                row.push(Cell::Integer(0));
            }
            rows.push(row);
        }
    }
    else if let Some(view) = view_def(exec.catalog, schema, table) {
        // SQLite infers a view column type from a plain column reference; this
        // engine reports the output names with an empty type.
        let (names, _) = exec.view_output(view)?;
        for (position, name) in names.iter().enumerate() {
            let mut row = vec![
                Cell::Integer(position as i64),
                Cell::Text(name.clone()),
                Cell::Text(String::new()),
                Cell::Integer(0),
                Cell::Null,
                Cell::Integer(0),
            ];
            if extended {
                row.push(Cell::Integer(0));
            }
            rows.push(row);
        }
    }
    Ok(ExecResult { statement: "pragma", columns, rows, affected: 0 })
}

/// `PRAGMA index_list(t)`: explicit indexes newest first, then the automatic
/// constraint indexes in reverse numbering order.
pub fn index_list(catalog: &SqliteCatalog, table: &str) -> ExecResult {
    let key = table.to_ascii_lowercase();
    let mut explicit: Vec<&IndexDef> = catalog.indexes.values().filter(|index| index.table == key).collect();
    explicit.sort_by(|left, right| right.order.cmp(&left.order));
    let mut rows = Vec::new();
    for index in explicit {
        rows.push(vec![
            Cell::Integer(rows.len() as i64),
            Cell::Text(index.name.clone()),
            Cell::Integer(i64::from(index.unique)),
            Cell::Text("c".to_string()),
            Cell::Integer(0),
        ]);
    }
    if let Some(def) = table_def(catalog, None, table) {
        for (number, _, origin) in autoindex_definitions(def).into_iter().rev() {
            rows.push(vec![
                Cell::Integer(rows.len() as i64),
                Cell::Text(format!("sqlite_autoindex_{}_{}", def.name, number)),
                Cell::Integer(1),
                Cell::Text(origin.to_string()),
                Cell::Integer(0),
            ]);
        }
    }
    ExecResult {
        statement: "pragma",
        columns: vec!["seq".to_string(), "name".to_string(), "unique".to_string(), "origin".to_string(), "partial".to_string()],
        rows,
        affected: 0,
    }
}

/// `PRAGMA index_info(i)`: `seqno`, `cid` and the column name of every key
/// column, in key order.
pub fn index_info(catalog: &SqliteCatalog, index: &str) -> ExecResult {
    let mut rows = Vec::new();
    if let Some(definition) = catalog.indexes.get(&index.to_ascii_lowercase()) {
        if let Some(def) = table_def(catalog, None, &definition.table) {
            for (seqno, position) in definition.columns.iter().enumerate() {
                rows.push(vec![Cell::Integer(seqno as i64), Cell::Integer(*position as i64), Cell::Text(def.columns[*position].name.clone())]);
            }
        }
    }
    else {
        for def in catalog.tables.iter().chain(catalog.temp_tables.iter().map(|temp| &temp.def)) {
            for (number, positions, _) in autoindex_definitions(def) {
                if !format!("sqlite_autoindex_{}_{}", def.name, number).eq_ignore_ascii_case(index) {
                    continue;
                }
                for (seqno, position) in positions.iter().enumerate() {
                    rows.push(vec![
                        Cell::Integer(seqno as i64),
                        Cell::Integer(*position as i64),
                        Cell::Text(def.columns[*position].name.clone()),
                    ]);
                }
            }
        }
    }
    ExecResult { statement: "pragma", columns: vec!["seqno".to_string(), "cid".to_string(), "name".to_string()], rows, affected: 0 }
}

/// `PRAGMA foreign_key_list(t)`: SQLite prepends each new key, so the list is
/// newest first.
pub fn foreign_key_list(catalog: &SqliteCatalog, table: &str) -> ExecResult {
    let mut rows = Vec::new();
    if let Some(def) = table_def(catalog, None, table) {
        for (id, key) in def.foreign_keys.iter().rev().enumerate() {
            for (seq, position) in key.columns.iter().enumerate() {
                rows.push(vec![
                    Cell::Integer(id as i64),
                    Cell::Integer(seq as i64),
                    Cell::Text(key.parent_display.clone()),
                    Cell::Text(def.columns[*position].name.clone()),
                    key.parent_columns.get(seq).map(|name| Cell::Text(name.clone())).unwrap_or(Cell::Null),
                    Cell::Text(key.on_update.clone()),
                    Cell::Text(key.on_delete.clone()),
                    Cell::Text("NONE".to_string()),
                ]);
            }
        }
    }
    ExecResult {
        statement: "pragma",
        columns: vec![
            "id".to_string(),
            "seq".to_string(),
            "table".to_string(),
            "from".to_string(),
            "to".to_string(),
            "on_update".to_string(),
            "on_delete".to_string(),
            "match".to_string(),
        ],
        rows,
        affected: 0,
    }
}
