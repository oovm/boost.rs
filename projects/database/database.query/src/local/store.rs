//! Table row persistence inside the local record map.

use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

use database_formats::yy::row::{decode_row, encode_row, select_stored};
use database_types::{Error, Result, Value, Vector};

pub use database_formats::yy::row::{
    ObjectResolver, ROW_PREFIX, RowReferences, row_key, row_references, select_encoded, table_prefix, with_object_resolver,
};

use super::ops::QueryRow;

/// Storage hook that decides where a written `bytes` value is kept.
///
/// The octet threshold is a physical property of the host, so the query layer
/// never picks one: the host installs a hook and receives either the original
/// inline octets or an object reference it created itself.
pub trait BytesPlacementHook: Send + Sync {
    /// Returns the value to store: inline octets or a reference into the host object store.
    fn place(&self, octets: Vec<u8>) -> Result<Value>;
}

/// Storage hook that decides where a written `vector<N>` value is kept.
///
/// Small vectors stay inline in the row's tagged JSON form; larger ones become
/// a content-addressed object reference (`ObjectKind::VectorPayload`). The
/// dimension threshold belongs to the host (same design as `bytes`), so the
/// query layer never picks one.
pub trait VectorPlacementHook: Send + Sync {
    /// Returns the value to store: inline `Value::Vector` or `Value::Object`
    /// referencing a `VectorPayload` in the host object store.
    fn place(&self, vector: Vector) -> Result<Value>;
}

type SharedPlacementHook = Arc<dyn BytesPlacementHook>;
type SharedVectorHook = Arc<dyn VectorPlacementHook>;

thread_local! {
    /// Placement hook installed for the duration of one write execution on this thread.
    static BYTES_PLACEMENT: RefCell<Option<SharedPlacementHook>> = RefCell::new(None);
    /// Vector placement hook installed for the duration of one write execution on this thread.
    static VECTOR_PLACEMENT: RefCell<Option<SharedVectorHook>> = RefCell::new(None);
}

struct PlacementGuard(Option<SharedPlacementHook>);

impl Drop for PlacementGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        BYTES_PLACEMENT.with(|slot| {
            slot.replace(previous);
        });
    }
}

struct VectorPlacementGuard(Option<SharedVectorHook>);

impl Drop for VectorPlacementGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        VECTOR_PLACEMENT.with(|slot| {
            slot.replace(previous);
        });
    }
}

/// Runs `body` with `hook` installed for `bytes` placement on this thread.
///
/// Without a hook every `bytes` value stays inline, which is what a host with
/// no object store can actually keep.
pub fn with_bytes_placement<T>(hook: Option<SharedPlacementHook>, body: impl FnOnce() -> T) -> T {
    let previous = BYTES_PLACEMENT.with(|slot| slot.replace(hook));
    let _guard = PlacementGuard(previous);
    body()
}

/// Runs `body` with `hook` installed for `vector<N>` placement on this thread.
///
/// Without a hook every vector stays inline in the row's tagged JSON form,
/// which matches hosts with no object store.
pub fn with_vector_placement<T>(hook: Option<SharedVectorHook>, body: impl FnOnce() -> T) -> T {
    let previous = VECTOR_PLACEMENT.with(|slot| slot.replace(hook));
    let _guard = VectorPlacementGuard(previous);
    body()
}

/// Places one `bytes` value through the installed hook.
pub(crate) fn place_bytes(octets: Vec<u8>) -> Result<Value> {
    BYTES_PLACEMENT.with(|slot| match slot.borrow().as_ref() {
        Some(hook) => hook.place(octets),
        None => Ok(Value::Bytes(octets)),
    })
}

/// Places one `vector<N>` value through the installed hook.
///
/// Without a hook the vector stays inline as `Value::Vector`; a hook may
/// return a `Value::Object` referencing a CAS `VectorPayload`.
pub(crate) fn place_vector(vector: Vector) -> Result<Value> {
    VECTOR_PLACEMENT.with(|slot| match slot.borrow().as_ref() {
        Some(hook) => hook.place(vector),
        None => Ok(Value::Vector(vector)),
    })
}

/// One record-map row borrowed from a storage window.
///
/// The borrow is bounded by the [`RowSource`] that handed it out: a pipeline
/// decodes it into an owned [`QueryRow`] and never keeps the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowedRow<'a> {
    /// Logical record name.
    pub key: &'a str,
    /// Stored row bytes, borrowed from the page image that holds them.
    pub bytes: &'a [u8],
}

/// Storage view that hands out borrowed rows for one record-key prefix.
///
/// A narrowed read keeps row bytes in the storage window instead of copying
/// them into a record map. The engine owns that window and passes it here for
/// the duration of one statement execution; every row is decoded into an owned
/// [`QueryRow`] before it leaves the operator.
pub trait RowSource {
    /// Rows whose record name starts with `prefix`, in ascending key order.
    ///
    /// The iterator borrows `self`, so the source must outlive it. It owns its
    /// copy of `prefix`, so the caller may pass a temporary string.
    fn rows_with_prefix<'a>(&'a self, prefix: &str) -> Box<dyn Iterator<Item = Result<BorrowedRow<'a>>> + 'a>;

    /// Exact row lookup by full record key.
    ///
    /// Defaults to the first row of [`Self::rows_with_prefix`]: a source yields
    /// ascending keys, so a longer key sharing the requested prefix means the
    /// exact row is absent.
    fn row_at<'a>(&'a self, key: &str) -> Option<Result<BorrowedRow<'a>>> {
        let first = self.rows_with_prefix(key).next()?;
        match first {
            Ok(row) if row.key == key => Some(Ok(row)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

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
    pub fn load_table(&self, table: &str) -> Result<Vec<QueryRow>> {
        match self.source {
            Some(source) => load_table_borrowed(source, table),
            None => load_table(self.records, table),
        }
    }

    /// Decode one row addressed by table and primary key.
    pub fn load_row(&self, table: &str, pk: &str) -> Result<QueryRow> {
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
    pub fn load_row_optional(&self, table: &str, pk: &str) -> Result<Option<QueryRow>> {
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

fn load_table_borrowed(source: &dyn RowSource, table: &str) -> Result<Vec<QueryRow>> {
    let prefix = table_prefix(table);
    let mut rows = Vec::new();
    for record in source.rows_with_prefix(&prefix) {
        rows.push(decode_row(record?.bytes)?);
    }
    Ok(rows)
}

/// Load all rows for `table` from the record map.
pub fn load_table(records: &BTreeMap<String, Vec<u8>>, table: &str) -> Result<Vec<QueryRow>> {
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
pub fn load_table_entries(records: &BTreeMap<String, Vec<u8>>, table: &str) -> Result<Vec<(String, QueryRow)>> {
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
/// [`QueryRow`]: decoding the whole record would allocate a map and one string
/// per column for each row. This walks the record layout once, captures the
/// requested value slices and hands them to `visit` while the record is still
/// borrowed, so a per-row check costs no allocation beyond one reusable buffer.
///
/// A column that is absent or stored as `NULL` yields `None`, matching
/// [`QueryRow::get`] returning `None`/`Value::Null`.
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
pub fn upsert_row(records: &mut BTreeMap<String, Vec<u8>>, table: &str, pk: &str, row: &QueryRow) -> Result<()> {
    records.insert(row_key(table, pk), encode_row(row)?);
    Ok(())
}

/// Load one row by primary key.
pub fn load_row(records: &BTreeMap<String, Vec<u8>>, table: &str, pk: &str) -> Result<QueryRow> {
    let key = row_key(table, pk);
    let bytes = records.get(&key).ok_or_else(|| missing_row(table, pk))?;
    decode_row(bytes)
}
