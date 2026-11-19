//! Uniqueness enforcement for the Phase 1 row store.
//!
//! Constraints come from two places: `@` (`unique`) attributes of the VOS
//! document and rules a host declares explicitly for composite column sets.
//! Both are expressed as [`TableUniqueConstraint`] so the write path has one
//! check. A conflict returns [`database_types::Error::UniqueViolation`] and leaves
//! the record map untouched.

use std::collections::BTreeMap;

use database_formats::yy::row::{row_key, select_encoded};
use database_storage::local::{load_table, scan_selected};
use database_types::{Error, Result, TableUniqueConstraint, UniqueConstraint, Value};
use schema_types::{ResolvedContract, TypeContractKind};

use super::{catalog, ops::QueryRow};

/// Connection-scoped unique-key index maintained by the VOS write path.
///
/// A uniqueness rule is otherwise checked by scanning every stored row of the
/// table, which makes a bulk import quadratic: the Nth insert re-reads N rows.
/// This index maps `(constraint, key) -> pk text` in memory so a write pays one
/// hash lookup instead of a table scan, and it is the reason a rule-bearing
/// table can still narrow to its exact record keys.
///
/// The index is never persisted, so it never touches the record windows a
/// statement publishes. It is authoritative only while its schema basis matches
/// the active contract, and every path that changes rows without maintaining it
/// — SQL statements, raw record writes, rollbacks — must invalidate it. A cold
/// index rebuilds from the full table the next time a complete write runs.
#[derive(Debug, Default)]
pub struct UniqueIndex {
    tables: std::sync::Mutex<std::collections::HashMap<String, TableIndex>>,
}

#[derive(Debug)]
struct TableIndex {
    basis: (String, u64),
    entries: std::collections::HashMap<(String, String), String>,
}

/// One write's index context: which connection cache to maintain, against
/// which schema basis, and whether the record map can rebuild it.
pub struct WriteIndex<'a> {
    /// The cache to check and maintain.
    pub index: &'a UniqueIndex,
    /// Schema basis the entries describe: document fingerprint and revision.
    pub basis: (String, u64),
    /// Whether the record map the write runs against holds every row of the
    /// table. Rebuilding a cold index from a partial map would silently miss
    /// duplicates, so it is only sound when this is `true`.
    pub complete: bool,
}

impl UniqueIndex {
    /// An empty cache; the first rule-bearing write rebuilds what it needs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached table index.
    pub fn invalidate_all(&self) {
        self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Whether the cached index for `table` still describes `basis`.
    pub fn is_warm(&self, table: &str, basis: (&str, u64)) -> bool {
        let tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        tables.get(table).is_some_and(|index| index.basis.0 == basis.0 && index.basis.1 == basis.1)
    }

    /// Check `row` against every rule on `table` and record its keys.
    ///
    /// `old` is the row being replaced: its keys are removed first, so a
    /// rewrite that keeps one value never collides with itself. A cold index
    /// rebuilds from `records` only when `ctx.complete` proves the map holds
    /// every row; otherwise the caller must have taken the full path and this
    /// fails loudly instead of trusting a partial scan.
    pub fn enforce(
        &self,
        ctx: &WriteIndex<'_>,
        records: &BTreeMap<String, Vec<u8>>,
        constraints: &[TableUniqueConstraint],
        table: &str,
        pk_field: &str,
        pk_text: &str,
        old: Option<&QueryRow>,
        row: &QueryRow,
    ) -> Result<()> {
        let applicable: Vec<&UniqueConstraint> =
            constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
        if applicable.is_empty() {
            return Ok(());
        }
        let mut tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = tables.get(table).is_none_or(|index| index.basis != ctx.basis);
        if stale {
            if !ctx.complete {
                return Err(Error::Unsupported(
                    "the unique-key index is cold and the record map is partial; the caller must take the full path",
                ));
            }
            let mut entries = std::collections::HashMap::new();
            for existing in load_table(records, table)? {
                let Some(pk) = existing.get(pk_field) else {
                    continue;
                };
                let Ok(existing_pk) = value_text(pk) else {
                    continue;
                };
                for constraint in &applicable {
                    if let Some(key) = entry_key(&existing, constraint) {
                        entries.insert(key, existing_pk.clone());
                    }
                }
            }
            tables.insert(table.to_owned(), TableIndex { basis: ctx.basis.clone(), entries });
        }
        let index = tables.get_mut(table).expect("index inserted or found above");
        if let Some(old_row) = old {
            retract_keys(index, &applicable, old_row, pk_text);
        }
        for constraint in &applicable {
            let Some(key) = entry_key(row, constraint) else {
                continue;
            };
            if let Some(owner) = index.entries.get(&key)
                && owner != pk_text
            {
                return Err(Error::UniqueViolation {
                    table: table.to_owned(),
                    constraint: constraint.name().to_owned(),
                    columns: constraint.columns().to_vec(),
                });
            }
        }
        for constraint in &applicable {
            if let Some(key) = entry_key(row, constraint) {
                index.entries.insert(key, pk_text.to_owned());
            }
        }
        Ok(())
    }

    /// Remove a deleted row's keys, keeping the index free of entries that
    /// would block a later reinsertion of the same value.
    pub fn retract(&self, ctx: &WriteIndex<'_>, constraints: &[TableUniqueConstraint], table: &str, pk_text: &str, old: &QueryRow) {
        let applicable: Vec<&UniqueConstraint> =
            constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
        if applicable.is_empty() {
            return;
        }
        let mut tables = self.tables.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(index) = tables.get_mut(table)
            && index.basis == ctx.basis
        {
            retract_keys(index, &applicable, old, pk_text);
        }
    }
}

fn retract_keys(index: &mut TableIndex, applicable: &[&UniqueConstraint], row: &QueryRow, pk_text: &str) {
    for constraint in applicable {
        if let Some(key) = entry_key(row, constraint)
            && index.entries.get(&key).map(String::as_str) == Some(pk_text)
        {
            index.entries.remove(&key);
        }
    }
}

/// The index slot one row occupies under `constraint`, `None` when its NULL
/// components make it distinct from every other row.
fn entry_key(row: &QueryRow, constraint: &UniqueConstraint) -> Option<(String, String)> {
    let (key, has_null) = constraint_key(row, constraint.columns());
    if has_null && constraint.nulls_distinct() {
        return None;
    }
    Some((constraint.name().to_owned(), key))
}

/// Derives single-column unique constraints declared with the VOS `@` attribute.
pub fn from_contract(contract: &ResolvedContract) -> Result<Vec<TableUniqueConstraint>> {
    let mut constraints = Vec::new();
    for entry in contract.types.iter().filter(|entry| entry.kind == TypeContractKind::Table) {
        let table = catalog::table_storage_name(entry)?.to_string();
        let type_name = entry.canonical_path.last().cloned().unwrap_or_else(|| table.clone());
        for field in &entry.fields {
            if field.attributes.iter().any(|attribute| attribute.name == "unique") {
                let constraint =
                    UniqueConstraint::new(format!("{type_name}_{}_key", field.canonical_name), vec![field.canonical_name.clone()], true)
                        .map_err(Error::from)?;
                constraints.push(TableUniqueConstraint::new(table.clone(), constraint).map_err(Error::from)?);
            }
        }
    }
    Ok(constraints)
}

/// Combines document-derived rules with host declarations, deduplicated by
/// `(table, name)` with document rules taking precedence.
pub fn merge(document: Vec<TableUniqueConstraint>, declared: &[TableUniqueConstraint]) -> Vec<TableUniqueConstraint> {
    let mut merged = document;
    for candidate in declared {
        let duplicate = merged
            .iter()
            .any(|existing| existing.table() == candidate.table() && existing.constraint().name() == candidate.constraint().name());
        if !duplicate {
            merged.push(candidate.clone());
        }
    }
    merged
}

/// Whether `table` carries any uniqueness rule at all.
pub fn has_rules(constraints: &[TableUniqueConstraint], table: &str) -> bool {
    constraints.iter().any(|candidate| candidate.table() == table)
}

/// Rejects `row` when it would duplicate an existing key of any constraint on `table`.
///
/// `row` replaces the row with `pk_text`, so that row never conflicts with itself.
/// NULL components follow the constraint's `nulls_distinct` flag and missing
/// fields count as NULL.
///
/// The comparison runs on the tagged bytes of the constraint columns instead of
/// decoded rows: a bulk load inserts thousands of rows into the same table, and
/// decoding each stored row into a map would make every insert cost a full table
/// decode. Byte equality of one column is exactly value equality, so the rule
/// does not get weaker.
pub fn check(
    records: &BTreeMap<String, Vec<u8>>,
    constraints: &[TableUniqueConstraint],
    table: &str,
    _pk_field: &str,
    pk_text: &str,
    row: &QueryRow,
) -> Result<()> {
    let applicable: Vec<&UniqueConstraint> =
        constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
    if applicable.is_empty() {
        return Ok(());
    }
    let target_key = row_key(table, pk_text);

    for constraint in applicable {
        let names: Vec<&str> = constraint.columns().iter().map(String::as_str).collect();
        let incoming = select_encoded(row, &names)?;
        if incoming.iter().any(Option::is_none) && constraint.nulls_distinct() {
            continue;
        }
        let mut violation = false;
        scan_selected(records, table, &names, |key, stored| {
            if key == target_key {
                return Ok(());
            }
            if stored.iter().any(Option::is_none) && constraint.nulls_distinct() {
                return Ok(());
            }
            if stored.iter().zip(&incoming).all(|(stored, incoming)| *stored == incoming.as_deref()) {
                violation = true;
            }
            Ok(())
        })?;
        if violation {
            return Err(Error::UniqueViolation {
                table: table.to_owned(),
                constraint: constraint.name().to_owned(),
                columns: constraint.columns().to_vec(),
            });
        }
    }
    Ok(())
}

/// Same rule as [`check`], evaluated against rows the caller already loaded.
///
/// Multi-row `INSERT` uses this to test a staged row against the stored rows
/// once and against the earlier rows of the same statement without reloading.
pub fn check_against_rows(
    existing_rows: &[QueryRow],
    constraints: &[TableUniqueConstraint],
    table: &str,
    pk_field: &str,
    pk_text: &str,
    row: &QueryRow,
) -> Result<()> {
    let applicable: Vec<&UniqueConstraint> =
        constraints.iter().filter(|candidate| candidate.table() == table).map(TableUniqueConstraint::constraint).collect();
    if applicable.is_empty() {
        return Ok(());
    }

    for constraint in applicable {
        let (incoming, incoming_null) = constraint_key(row, constraint.columns());
        if incoming_null && constraint.nulls_distinct() {
            continue;
        }
        for existing in existing_rows {
            let existing_pk = existing.get(pk_field).map(value_text).transpose()?.unwrap_or_default();
            if existing_pk == pk_text {
                continue;
            }
            let (candidate, candidate_null) = constraint_key(existing, constraint.columns());
            if candidate_null && constraint.nulls_distinct() {
                continue;
            }
            if incoming == candidate {
                return Err(Error::UniqueViolation {
                    table: table.to_owned(),
                    constraint: constraint.name().to_owned(),
                    columns: constraint.columns().to_vec(),
                });
            }
        }
    }
    Ok(())
}

/// Whether `incoming` would collide with `existing` under `constraint`.
///
/// Both key halves are compared, and a key that contains `NULL` never collides
/// when the constraint declares NULLs as distinct — the same rule [`check`]
/// applies, exposed so an `ON CONFLICT` clause can detect the row it targets.
pub fn keys_conflict(constraint: &UniqueConstraint, existing: &QueryRow, incoming: &QueryRow) -> bool {
    let (candidate, candidate_null) = constraint_key(existing, constraint.columns());
    if candidate_null && constraint.nulls_distinct() {
        return false;
    }
    let (key, incoming_null) = constraint_key(incoming, constraint.columns());
    if incoming_null && constraint.nulls_distinct() {
        return false;
    }
    candidate == key
}

fn constraint_key(row: &QueryRow, columns: &[String]) -> (String, bool) {
    let mut parts = Vec::with_capacity(columns.len());
    let mut has_null = false;
    for column in columns {
        match row.get(column) {
            None | Some(Value::Null) => {
                has_null = true;
                parts.push(Value::Null.key_component());
            }
            Some(value) => parts.push(value.key_component()),
        }
    }
    (parts.join("\u{1f}"), has_null)
}

/// Renders a value as its canonical text form for key comparison.
pub fn value_text(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Text(text) => text.clone(),
        Value::Uuid(id) => id.to_string(),
        Value::I64(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Date(date) => date.to_iso(),
        Value::Time(time) => time.to_iso(),
        Value::Timestamp(timestamp) => timestamp.to_iso(),
        Value::Decimal(decimal) => decimal.to_plain_string(),
        Value::Null => return Err(Error::Schema { message: "primary key cannot be null".into() }),
        _ => return Err(Error::Schema { message: "primary key must be a scalar value".into() }),
    })
}
