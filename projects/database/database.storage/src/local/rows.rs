//! Table-row access over one record map.
//!
//! A record map is the owned `name -> stored bytes` view a caller publishes
//! through a [`Snapshot`](super::Snapshot). Rows live under the row record
//! prefix, and this module is the one place that turns a record name, a prefix
//! range or a borrowed window into decoded rows. The byte layout itself belongs
//! to `database_formats::yy::row`; nothing here interprets a row record.

use std::collections::BTreeMap;

use database_formats::yy::row::{decode_row, encode_row, row_key, select_stored, table_prefix};
use database_types::{Error, Result, Value};

use super::snapshot::{RowRecord, RowSource};

/// One decoded table row.
pub type Row = BTreeMap<String, Value>;

/// Record rows one read pipeline can scan.
///
/// A full read owns every row in the map. A narrowed read owns only metadata
/// rows and borrows the statement's window from [`RowSource`].
pub struct ReadRows<'a> {
    records: &'a BTreeMap<String, Vec<u8>>,
    source: Option<&'a dyn RowSource>,
}

impl<'a> ReadRows<'a> {
    /// Rows come from the owned record map only.
    pub fn owned(records: &'a BTreeMap<String, Vec<u8>>) -> Self {
        Self { records, source: None }
    }

    /// Rows come from `source`; `records` still answers metadata lookups.
    pub fn borrowed(records: &'a BTreeMap<String, Vec<u8>>, source: &'a dyn RowSource) -> Self {
        Self { records, source: Some(source) }
    }

    /// Decode every row of `table`, whichever side holds the bytes.
    pub fn load_table(&self, table: &str) -> Result<Vec<Row>> {
        match self.source {
            Some(source) => load_table_borrowed(source, table),
            None => load_table(self.records, table),
        }
    }

    /// Decode one row addressed by table and primary key.
    pub fn load_row(&self, table: &str, pk: &str) -> Result<Row> {
        let key = row_key(table, pk);
        match self.source {
            Some(source) => match source.row_at(&key) {
                Some(record) => decode_row(record?.bytes),
                None => Err(missing_row(table, pk)),
            },
            None => load_row(self.records, table, pk),
        }
    }

    /// Decode one row addressed by table and primary key, `None` when absent.
    ///
    /// A primary-key equality filter must answer "no rows" for a key that was
    /// never written, which is different from the missing-row error a keyed
    /// write raises.
    pub fn load_row_optional(&self, table: &str, pk: &str) -> Result<Option<Row>> {
        let key = row_key(table, pk);
        match self.source {
            Some(source) => match source.row_at(&key) {
                Some(record) => Ok(Some(decode_row(record?.bytes)?)),
                None => Ok(None),
            },
            None => match self.records.get(&key) {
                Some(bytes) => Ok(Some(decode_row(bytes)?)),
                None => Ok(None),
            },
        }
    }
}

fn missing_row(table: &str, pk: &str) -> Error {
    Error::Schema { message: format!("missing row `{table}` pk `{pk}`") }
}

fn load_table_borrowed(source: &dyn RowSource, table: &str) -> Result<Vec<Row>> {
    let prefix = table_prefix(table);
    let mut rows = Vec::new();
    for record in source.rows_with_prefix(&prefix) {
        rows.push(decode_row(record?.bytes)?);
    }
    Ok(rows)
}

/// Load all rows for `table` from the record map.
pub fn load_table(records: &BTreeMap<String, Vec<u8>>, table: &str) -> Result<Vec<Row>> {
    // A read never needs the record-map key, so it decodes straight out of the
    // prefix range instead of cloning one record name per row.
    let prefix = table_prefix(table);
    let mut rows = Vec::new();
    for (key, bytes) in records.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        rows.push(decode_row(bytes)?);
    }
    Ok(rows)
}

/// Load all rows for `table` together with their record-map keys.
///
/// Write operators need the key to delete or replace a row in place, so this
/// variant is the shared primitive and [`load_table`] is the read-only view.
pub fn load_table_entries(records: &BTreeMap<String, Vec<u8>>, table: &str) -> Result<Vec<(String, Row)>> {
    let prefix = table_prefix(table);
    let mut out = Vec::new();
    for (key, bytes) in records.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        out.push((key.clone(), decode_row(bytes)?));
    }
    Ok(out)
}

/// Remove the row stored under `key`. Returns whether a row was present.
pub fn remove_row(records: &mut BTreeMap<String, Vec<u8>>, key: &str) -> bool {
    records.remove(key).is_some()
}

/// Visit every stored row of `table` exposing only the tagged bytes of `names`.
///
/// Uniqueness checks need a handful of columns from every row but never a full
/// decoded row: decoding the whole record would allocate a map and one string
/// per column for each row. This walks the record layout once, captures the
/// requested value slices and hands them to `visit` while the record is still
/// borrowed, so a per-row check costs no allocation beyond one reusable buffer.
///
/// A column that is absent or stored as `NULL` yields `None`, matching
/// [`Row::get`] returning `None`/`Value::Null`.
pub fn scan_selected<'a, F>(records: &'a BTreeMap<String, Vec<u8>>, table: &str, names: &[&str], mut visit: F) -> Result<()>
where
    F: FnMut(&str, &[Option<&'a [u8]>]) -> Result<()>,
{
    let prefix = table_prefix(table);
    let mut selected: Vec<Option<&'a [u8]>> = vec![None; names.len()];
    for (key, bytes) in records.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        selected.iter_mut().for_each(|slot| *slot = None);
        select_stored(bytes, names, &mut selected)?;
        visit(key, &selected)?;
    }
    Ok(())
}

/// Upsert one logical row keyed by `pk`.
pub fn upsert_row(records: &mut BTreeMap<String, Vec<u8>>, table: &str, pk: &str, row: &Row) -> Result<()> {
    records.insert(row_key(table, pk), encode_row(row)?);
    Ok(())
}

/// Load one row by primary key.
pub fn load_row(records: &BTreeMap<String, Vec<u8>>, table: &str, pk: &str) -> Result<Row> {
    let key = row_key(table, pk);
    let bytes = records.get(&key).ok_or_else(|| missing_row(table, pk))?;
    decode_row(bytes)
}
