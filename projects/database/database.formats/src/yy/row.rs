//! `.yydb` 记录值中的表行记录编解码。
//!
//! 一行表记录以 `__yydb/v0/row/<table>/<pk>` 为记录名，值采用带版本前缀的二进
//! 制行格式：版本字节、标志、布局 revision、列数、列身份表、NULL 位图和逐个带
//! 标签的值。v4 起列身份是持久 `FieldId`，显示名由目录提供，因此改名与重排不
//! 再改变存量字节；只有无法从目录解析身份的动态记录才退回列名。
//! 旧 JSON 行格式、v1/v2/v3 二进制行格式均已退役，解码遇到非当前版本一律失败
//! 并提示重建，不猜测形状。
//!
//! 行内的 `bytes` 与 `vector<N>` 可能被宿主改写为对象引用；解码需要
//! [`ObjectResolver`]，由宿主在一条语句执行期间用 [`with_object_resolver`] 安装。
//!
//! 具名 `unite` / `structure` 值由 [`RowSchemaDirectory`] 决定物理表示：目录给出
//! 稳定判别值、布局 revision 与成员顺序，记录里不重复变体名或逐实例字段名。
//! 宿主在一条语句执行期间用 [`with_row_schema`] 安装目录，编解码都按该目录校验。

use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

use database_types::{
    Date, Decimal, Error, FileDescriptor, FileManifestRef, HashAlgo, ObjectKind, ObjectRef, Result, Time, Timestamp, Value, Vector,
};

/// Record-map key prefix that owns table rows.
///
/// Row keys are `__yydb/v0/row/<table>/<pk>`, and their encoded values carry
/// tagged object and file references that garbage collection must count.
pub const ROW_PREFIX: &str = "__yydb/v0/row/";

/// Version byte prefixed to every stored row record.
///
/// Row records are self-describing and versioned. A record whose first byte is
/// not [`ROW_CODEC_VERSION`] was written by a retired codec (the JSON encoding,
/// the v1 binary encoding which capped every field at 65535 bytes, or the v2
/// encoding that had no closed named-value layout), and decoding it fails closed
/// with a rebuild hint instead of guessing a shape.
///
/// Before 1.0 the project does not promise cross-version reads. A codec change
/// bumps this version and the previous version is rejected outright; callers
/// rebuild from source data instead of expecting a compatibility branch, a dual
/// write or a shim.
pub const ROW_CODEC_VERSION: u8 = 4;

/// Header flag: the column table carries durable `FieldId`s instead of names.
const ROW_FLAG_FIELD_IDS: u8 = 0b0000_0001;

/// Reads the octets behind an out-of-line `bytes` object reference.
///
/// The storage host owns placement, so the row codec asks it to resolve a
/// reference instead of knowing how objects are kept.
pub trait ObjectResolver: Send + Sync {
    /// Returns the stored octets, failing closed when the object is missing or corrupt.
    fn resolve(&self, object: &ObjectRef) -> Result<Vec<u8>>;
}

type SharedResolver = Arc<dyn ObjectResolver>;

thread_local! {
    /// Resolver installed for the duration of one statement execution on this thread.
    static OBJECT_RESOLVER: RefCell<Option<SharedResolver>> = const { RefCell::new(None) };
}

struct ResolverGuard(Option<SharedResolver>);

impl Drop for ResolverGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        OBJECT_RESOLVER.with(|slot| {
            slot.replace(previous);
        });
    }
}

/// Runs `body` with `resolver` installed for row decoding on this thread.
///
/// Rows whose `bytes` values were placed in the host object store cannot be
/// decoded without a resolver, so a host that stores references always installs
/// one. Without it, decoding a reference returns [`Error::Unsupported`] instead
/// of guessing or downgrading the value to text.
pub fn with_object_resolver<T>(resolver: Option<SharedResolver>, body: impl FnOnce() -> T) -> T {
    let previous = OBJECT_RESOLVER.with(|slot| slot.replace(resolver));
    let _guard = ResolverGuard(previous);
    body()
}

/// Physical layout of one value as the row codec reads it.
///
/// `Dynamic` means no named declaration applies: the tagged value keeps its
/// ordinary self-describing form. `List` applies the inner layout to every
/// element. `Named` refers to a [`RowSchemaDirectory`] entry by durable schema
/// identity, so a `class`, `structure` or `unite` column never has to repeat its
/// name or per-instance member names in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueLayout {
    /// Self-describing value with no named declaration.
    Dynamic,
    /// List whose elements follow the inner layout.
    List(Box<ValueLayout>),
    /// Named declaration, keyed by durable schema identity.
    Named(u64),
}

/// Layout of one named `class`, `structure` or `unite` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedValueLayout {
    /// Named domain value embedded as a record with member names.
    ///
    /// A class row stays name-tagged (its members are not positionally closed);
    /// the layout only tells the encoder how to represent declared members that
    /// are themselves `structure` or `unite` values.
    Class {
        /// Durable schema identity.
        schema_id: u64,
        /// Declared members in catalog order.
        fields: Vec<NamedFieldLayout>,
    },
    /// Closed field record with a positional layout.
    Structure {
        /// Durable schema identity.
        schema_id: u64,
        /// Layout revision; a change rejects stored records instead of misreading.
        revision: u32,
        /// Declared members in layout order.
        fields: Vec<NamedFieldLayout>,
    },
    /// Tagged union with a stable discriminant per variant.
    Unite {
        /// Durable schema identity.
        schema_id: u64,
        /// Layout revision derived from discriminant/payload shape.
        revision: u32,
        /// Declared variants in catalog order.
        variants: Vec<NamedVariantLayout>,
    },
}

impl NamedValueLayout {
    /// Durable schema identity of this named declaration.
    pub fn schema_id(&self) -> u64 {
        match self {
            Self::Class { schema_id, .. } | Self::Structure { schema_id, .. } | Self::Unite { schema_id, .. } => *schema_id,
        }
    }
}

/// One member of a named `class` or `structure` layout.
///
/// The durable `field_id` is the physical identity inside a stored record; the
/// display name is resolved through the directory, so renaming a member never
/// rewrites stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedFieldLayout {
    /// Durable field identity.
    pub field_id: u64,
    /// Member name in the catalog.
    pub name: String,
    /// Layout of the member value.
    pub ty: ValueLayout,
}

/// One variant of a named `unite` layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedVariantLayout {
    /// Stable discriminant (source-order index in the declaration).
    pub discriminant: i64,
    /// Closed payload members in catalog order.
    pub fields: Vec<NamedFieldLayout>,
}

/// Layout facts that belong to one published catalog revision.
///
/// A row record names the revision it was written under. Reads select the
/// matching layout from the directory, which is what lets a deferred migration
/// keep old rows readable while new rows use the current declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevisionLayout {
    revision: u32,
    columns: BTreeMap<String, ValueLayout>,
    column_ids: BTreeMap<String, u64>,
    fields: BTreeMap<u64, NamedFieldLayout>,
    named: BTreeMap<u64, NamedValueLayout>,
}

impl RevisionLayout {
    /// Builds one revision layout from column names, their durable identities
    /// and the named declarations the columns refer to.
    pub fn new(
        revision: u32,
        columns: BTreeMap<String, ValueLayout>,
        column_ids: BTreeMap<String, u64>,
        named: BTreeMap<u64, NamedValueLayout>,
    ) -> Self {
        let fields = column_ids
            .iter()
            .filter_map(|(name, field_id)| {
                columns.get(name).map(|ty| (*field_id, NamedFieldLayout { field_id: *field_id, name: name.clone(), ty: ty.clone() }))
            })
            .collect();
        Self { revision, columns, column_ids, fields, named }
    }

    /// Revision this layout was published under.
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Layout of one table column by display name.
    pub fn column(&self, name: &str) -> Option<&ValueLayout> {
        self.columns.get(name)
    }

    /// Durable identity of one table column.
    pub fn column_id(&self, name: &str) -> Option<u64> {
        self.column_ids.get(name).copied()
    }

    /// Column declaration by durable identity.
    pub fn field(&self, field_id: u64) -> Option<&NamedFieldLayout> {
        self.fields.get(&field_id)
    }

    /// Named declaration by durable schema identity.
    pub fn named(&self, schema_id: u64) -> Option<&NamedValueLayout> {
        self.named.get(&schema_id)
    }
}

/// Catalog-derived directory the row codec consults while encoding.
///
/// The current revision is used for writes; retained revisions let a deferred
/// migration decode rows written before the change without rewriting them.
/// Named layouts are keyed by durable schema identity, which every stored
/// `class`, `unite` and `structure` value carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowSchemaDirectory {
    current: RevisionLayout,
    history: BTreeMap<u32, RevisionLayout>,
}

impl RowSchemaDirectory {
    /// Builds a directory from column layouts and named declarations.
    ///
    /// The row revision is `0`; hosts that persist revisions use
    /// [`Self::revisioned`].
    pub fn new(columns: BTreeMap<String, ValueLayout>, named: BTreeMap<u64, NamedValueLayout>) -> Self {
        Self::revisioned(0, columns, BTreeMap::new(), named)
    }

    /// Builds a directory for one published revision.
    pub fn revisioned(
        revision: u32,
        columns: BTreeMap<String, ValueLayout>,
        column_ids: BTreeMap<String, u64>,
        named: BTreeMap<u64, NamedValueLayout>,
    ) -> Self {
        Self { current: RevisionLayout::new(revision, columns, column_ids, named), history: BTreeMap::new() }
    }

    /// Retains one earlier revision for deferred reads.
    pub fn with_history(
        mut self,
        revision: u32,
        columns: BTreeMap<String, ValueLayout>,
        column_ids: BTreeMap<String, u64>,
        named: BTreeMap<u64, NamedValueLayout>,
    ) -> Self {
        self.history.insert(revision, RevisionLayout::new(revision, columns, column_ids, named));
        self
    }

    /// Current revision used by writes.
    pub fn revision(&self) -> u32 {
        self.current.revision()
    }

    /// Layout of one table column, when the catalog declares a named type there.
    pub fn column(&self, name: &str) -> Option<&ValueLayout> {
        self.current.column(name)
    }

    /// Durable identity of one table column in the current revision.
    pub fn column_id(&self, name: &str) -> Option<u64> {
        self.current.column_id(name)
    }

    /// Layout of one named declaration in the current revision.
    pub fn named(&self, schema_id: u64) -> Option<&NamedValueLayout> {
        self.current.named(schema_id)
    }

    /// Layout selected by a stored row's revision, falling back to the current
    /// one when the revision is the one this directory was built for.
    fn layout_for(&self, revision: u32) -> Option<&RevisionLayout> {
        if revision == self.current.revision() { Some(&self.current) } else { self.history.get(&revision) }
    }
}

type SharedDirectory = Arc<RowSchemaDirectory>;

thread_local! {
    /// Named-value directory installed for the duration of one statement.
    static ROW_SCHEMA: RefCell<Option<SharedDirectory>> = const { RefCell::new(None) };
}

struct SchemaGuard(Option<SharedDirectory>);

impl Drop for SchemaGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        ROW_SCHEMA.with(|slot| {
            slot.replace(previous);
        });
    }
}

/// Runs `body` with `directory` installed for row encoding on this thread.
///
/// A record that carries a `unite` or closed `structure` value cannot be encoded
/// or decoded without its declaration, so a host that stores those values always
/// installs the directory for the statement. Without it, encoding a named value
/// fails closed instead of falling back to a name-tagged record that would lose
/// the discriminant.
pub fn with_row_schema<T>(directory: Option<SharedDirectory>, body: impl FnOnce() -> T) -> T {
    let previous = ROW_SCHEMA.with(|slot| slot.replace(directory));
    let _guard = SchemaGuard(previous);
    body()
}

fn current_row_schema() -> Option<SharedDirectory> {
    ROW_SCHEMA.with(|slot| slot.borrow().clone())
}

/// Encode one row with an explicit directory, without touching the ambient one.
pub fn encode_row_with_directory(row: &BTreeMap<String, Value>, directory: &RowSchemaDirectory) -> Result<Vec<u8>> {
    with_row_schema(Some(Arc::new(directory.clone())), || encode_row(row))
}

/// Decode one row with an explicit directory, without touching the ambient one.
pub fn decode_row_with_directory(bytes: &[u8], directory: &RowSchemaDirectory) -> Result<BTreeMap<String, Value>> {
    with_row_schema(Some(Arc::new(directory.clone())), || decode_row(bytes))
}

/// Encode one value with an explicit directory, without touching the ambient one.
pub fn encode_tagged_with_directory(value: &Value, directory: &RowSchemaDirectory) -> Result<Vec<u8>> {
    with_row_schema(Some(Arc::new(directory.clone())), || encode_tagged(value))
}

/// Decode one value with an explicit directory, without touching the ambient one.
pub fn decode_tagged_with_directory(bytes: &[u8], directory: &RowSchemaDirectory) -> Result<Value> {
    with_row_schema(Some(Arc::new(directory.clone())), || decode_tagged(bytes))
}

/// Build the storage key for one table row.
pub fn row_key(table: &str, pk: &str) -> String {
    format!("{ROW_PREFIX}{table}/{pk}")
}

/// Record-key prefix every row of `table` shares.
pub fn table_prefix(table: &str) -> String {
    format!("{ROW_PREFIX}{table}/")
}

/// One stored row record's out-of-line references.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowReferences {
    /// Objects the row stores directly.
    pub objects: Vec<ObjectRef>,
    /// File descriptors the row stores.
    pub files: Vec<FileDescriptor>,
}

/// Collect the out-of-line objects one stored row record points at.
///
/// The reference closure must see exactly what the row committed, so this reads
/// the same record layout the reader uses and never resolves an object.
pub fn row_references(bytes: &[u8]) -> Result<RowReferences> {
    let mut references = Some(RowReferences::default());
    parse_row(bytes, &mut references, false)?;
    references.take().ok_or(Error::Corrupt("row record reference walk produced no result"))
}

/// Encode one row as the binary row record format.
///
/// Layout: `version u8`, `flags u8`, `layout revision u32`, `column count u16`,
/// then either one `field id u64` per column or one `name length u32 + UTF-8
/// name`, then the NULL bitmap and one tagged value for every non-null column.
/// A directory that can resolve every column name to a durable field identity
/// writes ids; otherwise the record stays name-keyed so dynamic projections and
/// storage-level rows still round-trip.
pub fn encode_row(row: &BTreeMap<String, Value>) -> Result<Vec<u8>> {
    let column_count = u16::try_from(row.len()).map_err(|_| Error::Unsupported("query row has more than 65535 columns"))?;
    let directory = current_row_schema();
    let directory = directory.as_deref();
    let named = directory.and_then(|directory| {
        let mut identities = Vec::with_capacity(row.len());
        for name in row.keys() {
            identities.push((*directory.current.column_ids.get(name)?, name));
        }
        identities.sort_by_key(|(field_id, _)| *field_id);
        Some(identities)
    });
    let revision = directory.map_or(0, RowSchemaDirectory::revision);
    let mut out = Vec::with_capacity(8 + row.len() * 12);
    out.push(ROW_CODEC_VERSION);
    out.push(if named.is_some() { ROW_FLAG_FIELD_IDS } else { 0 });
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&column_count.to_le_bytes());
    let bitmap_len = row.len().div_ceil(8);
    let bitmap_start = out.len() + if named.is_some() { row.len() * 8 } else { row.iter().map(|(name, _)| 4 + name.len()).sum() };
    if let Some(identities) = &named {
        for (field_id, _) in identities {
            out.extend_from_slice(&field_id.to_le_bytes());
        }
    }
    else {
        for name in row.keys() {
            put_len(&mut out, name.len(), "column name")?;
            out.extend_from_slice(name.as_bytes());
        }
    }
    out.resize(bitmap_start + bitmap_len, 0);
    match &named {
        Some(identities) => {
            for (index, (_, name)) in identities.iter().enumerate() {
                let value = &row[*name];
                if matches!(value, Value::Null) {
                    out[bitmap_start + index / 8] |= 1 << (index % 8);
                }
            }
            for (_, name) in identities {
                let value = &row[*name];
                if matches!(value, Value::Null) {
                    continue;
                }
                encode_value(&mut out, value, column_layout(directory, name), directory, 0)?;
            }
        }
        None => {
            for (index, (_, value)) in row.iter().enumerate() {
                if matches!(value, Value::Null) {
                    out[bitmap_start + index / 8] |= 1 << (index % 8);
                }
            }
            for (name, value) in row {
                if matches!(value, Value::Null) {
                    continue;
                }
                encode_value(&mut out, value, column_layout(directory, name), directory, 0)?;
            }
        }
    }
    Ok(out)
}

/// Decode one stored row record, resolving out-of-line references.
///
/// Object references are resolved through the thread-local resolver installed by
/// [`with_object_resolver`]; a record that names one without a resolver fails
/// closed instead of returning a half-decoded row.
pub fn decode_row(bytes: &[u8]) -> Result<BTreeMap<String, Value>> {
    parse_row(bytes, &mut None, true)
}

/// Tagged bytes of `names` inside one in-memory row, in `names` order.
///
/// Values are encoded with the same codec the record uses, so equality of these
/// byte blobs is exactly equality of the stored values. `NULL` is `None`.
pub fn select_encoded(row: &BTreeMap<String, Value>, names: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
    let directory = current_row_schema();
    let directory = directory.as_deref();
    let mut selected = vec![None; names.len()];
    for (slot, name) in names.iter().enumerate() {
        let Some(value) = row.get(*name)
        else {
            continue;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        let mut encoded = Vec::new();
        encode_value(&mut encoded, value, column_layout(directory, name), directory, 0)?;
        selected[slot] = Some(encoded);
    }
    Ok(selected)
}

/// Tagged bytes of one value, the same form [`select_encoded`] answers per column.
pub fn encode_tagged(value: &Value) -> Result<Vec<u8>> {
    let directory = current_row_schema();
    let directory = directory.as_deref();
    let mut encoded = Vec::new();
    encode_value(&mut encoded, value, None, directory, 0)?;
    Ok(encoded)
}

/// Decode one payload previously produced by [`encode_tagged`].
///
/// This is the cell-level counterpart of [`decode_row`]: it decodes a single
/// stored value (for example a structured `any` column) without a field name or
/// record envelope.
pub fn decode_tagged(bytes: &[u8]) -> Result<Value> {
    let directory = current_row_schema();
    let mut cursor = RowCursor::new(bytes);
    let value = decode_value(&mut cursor, &mut None, false, directory.as_deref().map(|directory| &directory.current), 0)?;
    if cursor.position != bytes.len() {
        return Err(Error::Corrupt("tagged value has trailing bytes"));
    }
    Ok(value)
}

/// Fill `selected` with the tagged bytes of `names` from one stored record.
///
/// The slices borrow `bytes`, so a caller can compare columns of a stored row
/// without decoding the whole record into a map.
pub fn select_stored<'a>(bytes: &'a [u8], names: &[&str], selected: &mut [Option<&'a [u8]>]) -> Result<()> {
    match bytes.first() {
        Some(version) if *version == ROW_CODEC_VERSION => {}
        Some(_) => {
            return Err(Error::Unsupported(
                "row record uses a retired row encoding: rebuild the database from source data to use the current \
                 binary row format",
            ));
        }
        None => return Err(Error::Corrupt("row record is empty")),
    }
    let directory = current_row_schema();
    let directory = directory.as_deref();
    let mut cursor = RowCursor::new(bytes);
    cursor.take(1)?;
    let flags = cursor.u8()?;
    if flags & !ROW_FLAG_FIELD_IDS != 0 {
        return Err(Error::Corrupt("row record header flags are unknown"));
    }
    let revision = cursor.u32()?;
    let field_keyed = flags & ROW_FLAG_FIELD_IDS != 0;
    let layout = if field_keyed {
        Some(
            directory
                .ok_or_else(|| Error::Unsupported("selecting from a field-id keyed row needs the schema directory installed"))?
                .layout_for(revision)
                .ok_or_else(|| Error::Corrupt("row record uses a layout revision the directory does not declare"))?,
        )
    }
    else {
        None
    };
    let wanted: Vec<Option<u64>> = names.iter().map(|name| layout.and_then(|layout| layout.column_id(name))).collect();
    let column_count = cursor.u16()? as usize;
    let bitmap = cursor.take(column_count.div_ceil(8))?;
    // Two cursors over one record: one reads the identity/name table, the other
    // the values that follow, so a skipped value is never decoded.
    let keys_start = cursor;
    let mut keys_cursor = keys_start;
    if field_keyed {
        keys_cursor.take(column_count * 8)?;
    }
    else {
        for _ in 0..column_count {
            keys_cursor.skip_text()?;
        }
    }
    let mut values = keys_cursor;
    let mut keys_cursor = keys_start;
    for index in 0..column_count {
        let slot = if field_keyed {
            let field_id = keys_cursor.u64()?;
            wanted.iter().position(|wanted| *wanted == Some(field_id))
        }
        else {
            let name = keys_cursor.text_slice()?;
            names.iter().position(|wanted| *wanted == name)
        };
        // A NULL column carries no value bytes: the row bitmap already said so.
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            continue;
        }
        let start = values.position;
        let end = values.skip_value(0)?;
        if let Some(slot) = slot {
            selected[slot] = Some(&bytes[start..end]);
        }
    }
    if values.remaining() != 0 {
        return Err(Error::Corrupt("row record has trailing bytes"));
    }
    Ok(())
}

const TAG_NULL: u8 = 0x00;
const TAG_BOOL: u8 = 0x01;
const TAG_I64: u8 = 0x02;
const TAG_F64: u8 = 0x03;
const TAG_TEXT: u8 = 0x04;
const TAG_UUID: u8 = 0x05;
const TAG_DATE: u8 = 0x06;
const TAG_TIME: u8 = 0x07;
const TAG_TIMESTAMP: u8 = 0x08;
const TAG_DECIMAL: u8 = 0x09;
const TAG_BYTES: u8 = 0x0A;
const TAG_VECTOR: u8 = 0x0B;
const TAG_OBJECT: u8 = 0x0C;
const TAG_FILE: u8 = 0x0D;
const TAG_LIST: u8 = 0x0E;
/// 嵌套结构化行（`any` 值里的对象）：按成员名排序的键值序列。
const TAG_NESTED_ROW: u8 = 0x0F;
/// 无符号 64 位整数。JSON 里超过 `i64::MAX` 的整数解析成 [`Value::U64`]，
/// 行格式必须能原样往返，不能按有符号数截断。
const TAG_U64: u8 = 0x10;
/// 具名 `unite` 值：稳定 schema id、布局 revision、判别值和定位载荷。
///
/// 载荷按变体在目录中的成员顺序逐个编码，记录里不含变体名或成员名。读取时
/// 按目录校验 revision、判别值与载荷成员数，任一不符都 fail-closed。
const TAG_UNITE: u8 = 0x11;
/// 具名 `structure` 值：稳定 schema id、布局 revision、字段数、NULL 位图和
/// 按目录字段序编码的定位值。记录里不含逐实例字段名。
const TAG_STRUCTURE: u8 = 0x12;
/// 具名 `class` 值：稳定 schema id、成员数和按持久字段身份升序排列的成员。
///
/// 记录里不含成员名，改名与重排由目录解析，不改变存量字节。
const TAG_NAMED_ROW: u8 = 0x13;

/// Deepest list nesting the row codec encodes or decodes.
///
/// A corrupt or hostile record could otherwise nest lists until the stack
/// overflows, so both directions fail closed past this depth.
const MAX_LIST_DEPTH: u8 = 32;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn put_len(out: &mut Vec<u8>, len: usize, _what: &str) -> Result<()> {
    let len = u32::try_from(len).map_err(|_| Error::Unsupported("row record field exceeds 4294967295 bytes"))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn encode_value(
    out: &mut Vec<u8>,
    value: &Value,
    layout: Option<&ValueLayout>,
    directory: Option<&RowSchemaDirectory>,
    depth: u8,
) -> Result<()> {
    match value {
        // A row column's null is carried by the row null bitmap, but a list
        // element needs its own tag because there is no bitmap inside a list.
        Value::Null => {
            out.push(TAG_NULL);
        }
        Value::Bool(flag) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*flag));
        }
        Value::I64(integer) => {
            out.push(TAG_I64);
            out.extend_from_slice(&integer.to_le_bytes());
        }
        Value::U64(integer) => {
            out.push(TAG_U64);
            out.extend_from_slice(&integer.to_le_bytes());
        }
        Value::F64(number) => {
            if !number.is_finite() {
                return Err(Error::Unsupported("query row encoding rejects non-finite REAL values"));
            }
            out.push(TAG_F64);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::Text(text) => {
            out.push(TAG_TEXT);
            put_len(out, text.len(), "text value")?;
            out.extend_from_slice(text.as_bytes());
        }
        Value::Uuid(id) => {
            out.push(TAG_UUID);
            out.extend_from_slice(id.as_bytes());
        }
        Value::Date(date) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&date.days().to_le_bytes());
        }
        Value::Time(time) => {
            out.push(TAG_TIME);
            out.extend_from_slice(&time.nanos().to_le_bytes());
        }
        Value::Timestamp(timestamp) => {
            out.push(TAG_TIMESTAMP);
            out.extend_from_slice(&timestamp.unix_micros().to_le_bytes());
        }
        Value::Decimal(decimal) => {
            out.push(TAG_DECIMAL);
            out.extend_from_slice(&decimal.unscaled().to_le_bytes());
            out.push(decimal.precision());
            out.push(decimal.scale());
        }
        Value::Bytes(octets) => {
            out.push(TAG_BYTES);
            put_len(out, octets.len(), "bytes value")?;
            out.extend_from_slice(octets);
        }
        Value::Vector(vector) => {
            out.push(TAG_VECTOR);
            let dimension = u16::try_from(vector.dim).map_err(|_| Error::Unsupported("inline row vector dimension exceeds 65535"))?;
            let payload_len =
                u32::try_from(dimension as usize * 4).map_err(|_| Error::Unsupported("inline row vector payload exceeds 4 GiB"))?;
            out.extend_from_slice(&dimension.to_le_bytes());
            out.extend_from_slice(&payload_len.to_le_bytes());
            for component in vector.data.iter() {
                out.extend_from_slice(&component.to_le_bytes());
            }
        }
        Value::Object(object) => {
            out.push(TAG_OBJECT);
            out.push(algo_tag(object.algo)?);
            out.push(object_kind_tag(object.kind)?);
            out.extend_from_slice(&object.hash);
            out.extend_from_slice(&object.size.to_le_bytes());
        }
        Value::File(file) => {
            out.push(TAG_FILE);
            let digest = parse_digest_hex(&file.digest).ok_or(Error::Corrupt("file descriptor digest is not 64 hexadecimal digits"))?;
            out.extend_from_slice(&digest);
            out.extend_from_slice(&file.octets.to_le_bytes());
            let mut flags = 0_u8;
            if file.name.is_some() {
                flags |= 0b01;
            }
            if file.media_type.is_some() {
                flags |= 0b10;
            }
            out.push(flags);
            if let Some(name) = &file.name {
                put_len(out, name.len(), "file name")?;
                out.extend_from_slice(name.as_bytes());
            }
            if let Some(media_type) = &file.media_type {
                put_len(out, media_type.len(), "file media type")?;
                out.extend_from_slice(media_type.as_bytes());
            }
            out.extend_from_slice(&file.manifest.hash);
            out.extend_from_slice(&file.manifest.size.to_le_bytes());
        }
        Value::Row(fields) => match named_layout(layout, directory)? {
            Some(NamedValueLayout::Class { schema_id, fields: declared }) => {
                encode_class(out, fields, *schema_id, declared, directory, depth)?;
            }
            Some(NamedValueLayout::Structure { schema_id, revision, fields: declared }) => {
                encode_structure(out, fields, *schema_id, *revision, declared, directory, depth)?;
            }
            Some(NamedValueLayout::Unite { .. }) => {
                return Err(Error::Schema {
                    message: "a `unite` value must carry a discriminant and payload, not a bare record".into(),
                });
            }
            None => {
                encode_dynamic_row(out, fields, directory, depth)?;
            }
        },
        Value::List(items) => {
            if let Some(NamedValueLayout::Unite { schema_id, revision, variants }) = named_layout(layout, directory)? {
                encode_unite(out, items, *schema_id, *revision, variants, directory, depth)?;
                return Ok(());
            }
            let element_layout = match layout {
                Some(ValueLayout::List(inner)) => Some(inner.as_ref()),
                _ => None,
            };
            out.push(TAG_LIST);
            let count = u32::try_from(items.len()).map_err(|_| Error::Unsupported("row record list has more than 4294967295 elements"))?;
            out.extend_from_slice(&count.to_le_bytes());
            let nested = deeper(depth, "row record list nests deeper than the supported depth")?;
            for item in items {
                encode_value(out, item, element_layout, directory, nested)?;
            }
        }
        _ => {
            return Err(Error::Unsupported(
                "query row encoding supports null, bool, i64, u64, real, text, uuid, bytes, vector, date, time, timestamp, \
                 decimal, bytes object references, file descriptors, lists, and nested rows only",
            ));
        }
    }
    Ok(())
}

/// Layout of one stored column, treating `Dynamic` as "no named declaration".
fn column_layout<'a>(directory: Option<&'a RowSchemaDirectory>, name: &str) -> Option<&'a ValueLayout> {
    match directory.and_then(|directory| directory.column(name)) {
        Some(ValueLayout::Dynamic) | None => None,
        Some(layout) => Some(layout),
    }
}

/// Resolves the named declaration a layout points at.
fn named_layout<'a>(layout: Option<&ValueLayout>, directory: Option<&'a RowSchemaDirectory>) -> Result<Option<&'a NamedValueLayout>> {
    let Some(ValueLayout::Named(schema_id)) = layout
    else {
        return Ok(None);
    };
    let Some(directory) = directory
    else {
        return Err(Error::Unsupported("encoding a named value needs the schema directory installed for the statement"));
    };
    directory
        .named(*schema_id)
        .map(Some)
        .ok_or_else(|| Error::Schema { message: format!("row encoding names schema id `{schema_id}`, which the directory does not declare") })
}

fn deeper(depth: u8, what: &'static str) -> Result<u8> {
    depth.checked_add(1).filter(|nested| *nested <= MAX_LIST_DEPTH).ok_or(Error::Unsupported(what))
}

/// Encodes a record with member names and no named declaration.
fn encode_dynamic_row(
    out: &mut Vec<u8>,
    fields: &BTreeMap<String, Value>,
    directory: Option<&RowSchemaDirectory>,
    depth: u8,
) -> Result<()> {
    out.push(TAG_NESTED_ROW);
    put_len(out, fields.len(), "nested row")?;
    let nested = deeper(depth, "nested row nests deeper than the supported depth")?;
    for (name, item) in fields {
        put_len(out, name.len(), "nested row field name")?;
        out.extend_from_slice(name.as_bytes());
        encode_value(out, item, None, directory, nested)?;
    }
    Ok(())
}

/// Encodes one `unite` value as a stable discriminant plus positional payload.
///
/// The canonical in-memory form is `[i64 discriminant, row payload]`. The record
/// carries the schema id, layout revision, discriminant and payload member count;
/// member names stay in the directory, so a rename never changes the bytes.
fn encode_unite(
    out: &mut Vec<u8>,
    items: &[Value],
    schema_id: u64,
    revision: u32,
    variants: &[NamedVariantLayout],
    directory: Option<&RowSchemaDirectory>,
    depth: u8,
) -> Result<()> {
    let [tag, payload] = items
    else {
        return Err(Error::Schema { message: "a `unite` value must be `[discriminant, payload]` with exactly two elements".into() });
    };
    let Value::I64(discriminant) = tag
    else {
        return Err(Error::Schema { message: "a `unite` discriminant must be an integer".into() });
    };
    let Value::Row(payload) = payload
    else {
        return Err(Error::Schema { message: "a `unite` payload must be a record".into() });
    };
    let variant = variants.iter().find(|variant| variant.discriminant == *discriminant).ok_or_else(|| Error::Schema {
        message: format!(
            "`{discriminant}` is not a declared `unite` variant ({})",
            variants.iter().map(|variant| variant.discriminant.to_string()).collect::<Vec<_>>().join(", ")
        ),
    })?;
    for name in payload.keys() {
        if !variant.fields.iter().any(|field| field.name == *name) {
            return Err(Error::Schema {
                message: format!("`unite` variant `{discriminant}` does not declare payload member `{name}`"),
            });
        }
    }
    for field in &variant.fields {
        if !payload.contains_key(&field.name) {
            return Err(Error::Schema {
                message: format!("`unite` variant `{discriminant}` is missing required payload member `{}`", field.name),
            });
        }
    }
    let member_count =
        u16::try_from(variant.fields.len()).map_err(|_| Error::Unsupported("unite variant has more than 65535 payload members"))?;
    out.push(TAG_UNITE);
    out.extend_from_slice(&schema_id.to_le_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&discriminant.to_le_bytes());
    out.extend_from_slice(&member_count.to_le_bytes());
    let nested = deeper(depth, "unite payload nests deeper than the supported depth")?;
    for field in &variant.fields {
        let value = payload.get(&field.name).expect("validated payload member");
        encode_value(out, value, Some(&field.ty), directory, nested)?;
    }
    Ok(())
}

/// Encodes one named `class` value with durable member identities.
///
/// The record names its schema id and one field id per present member; the
/// directory supplies the display names, so a member rename or reorder keeps
/// stored bytes valid.
fn encode_class(
    out: &mut Vec<u8>,
    fields: &BTreeMap<String, Value>,
    schema_id: u64,
    declared: &[NamedFieldLayout],
    directory: Option<&RowSchemaDirectory>,
    depth: u8,
) -> Result<()> {
    let mut present = Vec::with_capacity(fields.len());
    for name in fields.keys() {
        let field = declared
            .iter()
            .find(|field| field.name == *name)
            .ok_or_else(|| Error::Schema { message: format!("named class does not declare member `{name}`") })?;
        present.push(field);
    }
    present.sort_by_key(|field| field.field_id);
    let count = u16::try_from(present.len()).map_err(|_| Error::Unsupported("named class has more than 65535 members"))?;
    out.push(TAG_NAMED_ROW);
    out.extend_from_slice(&schema_id.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for field in &present {
        out.extend_from_slice(&field.field_id.to_le_bytes());
    }
    let nested = deeper(depth, "named class nests deeper than the supported depth")?;
    for field in present {
        let value = fields.get(&field.name).expect("present member");
        encode_value(out, value, Some(&field.ty), directory, nested)?;
    }
    Ok(())
}

/// Encodes one closed `structure` value positionally by durable field identity.
///
/// Members are written in ascending field-id order, not declaration order, so a
/// declaration reorder keeps both the layout revision and every stored byte.
/// The null bitmap marks absent or null members; undeclared members are refused
/// at the write boundary.
fn encode_structure(
    out: &mut Vec<u8>,
    fields: &BTreeMap<String, Value>,
    schema_id: u64,
    revision: u32,
    declared: &[NamedFieldLayout],
    directory: Option<&RowSchemaDirectory>,
    depth: u8,
) -> Result<()> {
    for name in fields.keys() {
        if !declared.iter().any(|field| field.name == *name) {
            return Err(Error::Schema { message: format!("closed `structure` does not declare member `{name}`") });
        }
    }
    let mut ordered: Vec<&NamedFieldLayout> = declared.iter().collect();
    ordered.sort_by_key(|field| field.field_id);
    let field_count = u16::try_from(ordered.len()).map_err(|_| Error::Unsupported("structure has more than 65535 members"))?;
    out.push(TAG_STRUCTURE);
    out.extend_from_slice(&schema_id.to_le_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&field_count.to_le_bytes());
    for field in &ordered {
        out.extend_from_slice(&field.field_id.to_le_bytes());
    }
    let bitmap_start = out.len();
    out.resize(bitmap_start + ordered.len().div_ceil(8), 0);
    for (index, field) in ordered.iter().enumerate() {
        if fields.get(&field.name).is_none_or(|value| matches!(value, Value::Null)) {
            out[bitmap_start + index / 8] |= 1 << (index % 8);
        }
    }
    let nested = deeper(depth, "structure nests deeper than the supported depth")?;
    for field in ordered {
        let Some(value) = fields.get(&field.name).filter(|value| !matches!(value, Value::Null))
        else {
            continue;
        };
        encode_value(out, value, Some(&field.ty), directory, nested)?;
    }
    Ok(())
}

fn algo_tag(algo: HashAlgo) -> Result<u8> {
    match algo {
        HashAlgo::Blake3 => Ok(0),
        _ => Err(Error::Unsupported("query row encoding supports the blake3 digest algorithm only")),
    }
}

fn object_kind_tag(kind: ObjectKind) -> Result<u8> {
    Ok(match kind {
        ObjectKind::Blob => 0,
        ObjectKind::VectorPayload => 1,
        ObjectKind::FileManifest => 2,
        ObjectKind::AnnSegment => 3,
        _ => return Err(Error::Unsupported("query row encoding does not support this object kind")),
    })
}

fn algo_from_tag(tag: u8) -> Result<HashAlgo> {
    match tag {
        0 => Ok(HashAlgo::Blake3),
        _ => Err(Error::Corrupt("row record digest algorithm tag is unknown")),
    }
}

fn object_kind_from_tag(tag: u8) -> Result<ObjectKind> {
    match tag {
        0 => Ok(ObjectKind::Blob),
        1 => Ok(ObjectKind::VectorPayload),
        2 => Ok(ObjectKind::FileManifest),
        3 => Ok(ObjectKind::AnnSegment),
        _ => Err(Error::Corrupt("row record object kind tag is unknown")),
    }
}

fn digest_hex(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0F) as usize] as char);
    }
    out
}

fn parse_digest_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in text.as_bytes().chunks(2).enumerate() {
        digest[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(digest)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_row(bytes: &[u8], references: &mut Option<RowReferences>, resolve: bool) -> Result<BTreeMap<String, Value>> {
    let directory = current_row_schema();
    let directory = directory.as_deref();
    match bytes.first() {
        Some(version) if *version == ROW_CODEC_VERSION => {}
        Some(_) => {
            return Err(Error::Unsupported(
                "row record uses a retired row encoding: rebuild the database from source data to use the current \
                 binary row format",
            ));
        }
        None => return Err(Error::Corrupt("row record is empty")),
    }
    let mut header = RowCursor::new(bytes);
    header.take(1)?;
    let flags = header.u8()?;
    if flags & !ROW_FLAG_FIELD_IDS != 0 {
        return Err(Error::Corrupt("row record header flags are unknown"));
    }
    let revision = header.u32()?;
    let column_count = header.u16()? as usize;
    let field_keyed = flags & ROW_FLAG_FIELD_IDS != 0;
    let revision_layout = if field_keyed {
        Some(
            directory
                .ok_or_else(|| Error::Unsupported("decoding a field-id keyed row needs the schema directory installed"))?
                .layout_for(revision)
                .ok_or_else(|| Error::Corrupt("row record uses a layout revision the directory does not declare"))?,
        )
    }
    else {
        directory.map(|directory| &directory.current)
    };
    let bitmap_len = column_count.div_ceil(8);
    // Keep the bitmap as a slice of the record and walk the identity table twice:
    // the first walk finds where values start, the second binds each value.
    let keys_start = header;
    let mut walk = keys_start;
    if field_keyed {
        walk.take(column_count * 8)?;
    }
    else {
        for _ in 0..column_count {
            walk.skip_text()?;
        }
    }
    let bitmap = walk.take(bitmap_len)?;
    let mut keys = keys_start;
    let mut values = walk;
    let mut row = BTreeMap::new();
    for index in 0..column_count {
        let name = if field_keyed {
            let field_id = keys.u64()?;
            let declared = revision_layout.ok_or_else(|| Error::Corrupt("row record names a field without a directory"))?;
            declared
                .field(field_id)
                .ok_or_else(|| Error::Corrupt("row record names a field id the directory does not declare"))?
                .name
                .clone()
        }
        else {
            keys.text()?
        };
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            row.insert(name, Value::Null);
            continue;
        }
        let value = decode_value(&mut values, references, resolve, revision_layout, 0)?;
        row.insert(name, value);
    }
    if values.remaining() != 0 {
        return Err(Error::Corrupt("row record has trailing bytes"));
    }
    Ok(row)
}

/// Bounds-checked forward reader over one row record.
///
/// `Copy` lets one recorded row be walked twice from two saved positions: one
/// cursor reads the name table, the other reads the values that follow it.
#[derive(Clone, Copy)]
struct RowCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> RowCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(len).filter(|end| *end <= self.bytes.len()).ok_or(Error::Corrupt("row record is truncated"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i128(&mut self) -> Result<i128> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn digest(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }

    fn text(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::Corrupt("row record text is not UTF-8"))
    }

    /// Borrow one length-prefixed UTF-8 string without allocating it.
    fn text_slice(&mut self) -> Result<&'a str> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take(len)?).map_err(|_| Error::Corrupt("row record text is not UTF-8"))
    }

    /// Advance past one tagged value without decoding it, returning the byte
    /// offset just past it.
    ///
    /// Mirrors [`decode_value`] tag for tag; a mismatch here would desynchronize
    /// a selected-column walk, so every tag the decoder accepts is covered.
    fn skip_value(&mut self, depth: u8) -> Result<usize> {
        match self.u8()? {
            TAG_NULL => {}
            TAG_BOOL => {
                self.take(1)?;
            }
            TAG_I64 | TAG_U64 | TAG_F64 | TAG_TIME | TAG_TIMESTAMP => {
                self.take(8)?;
            }
            TAG_TEXT => {
                let len = self.u32()? as usize;
                self.take(len)?;
            }
            TAG_UUID => {
                self.take(16)?;
            }
            TAG_DATE => {
                self.take(4)?;
            }
            TAG_DECIMAL => {
                self.take(18)?;
            }
            TAG_BYTES => {
                let len = self.u32()? as usize;
                self.take(len)?;
            }
            TAG_VECTOR => {
                let _dimension = self.u16()? as usize;
                let payload_len = self.u32()? as usize;
                self.take(payload_len)?;
            }
            TAG_OBJECT => {
                self.take(1 + 1 + 32 + 8)?;
            }
            TAG_FILE => {
                self.take(32)?;
                self.take(8)?;
                let flags = self.u8()?;
                if flags & !0b11 != 0 {
                    return Err(Error::Corrupt("row record file flags are invalid"));
                }
                if flags & 0b01 != 0 {
                    let len = self.u32()? as usize;
                    self.take(len)?;
                }
                if flags & 0b10 != 0 {
                    let len = self.u32()? as usize;
                    self.take(len)?;
                }
                self.take(32 + 8)?;
            }
            TAG_LIST => {
                let count = self.u32()? as usize;
                if count > self.remaining() {
                    return Err(Error::Corrupt("row record list length exceeds the remaining bytes"));
                }
                let nested = depth
                    .checked_add(1)
                    .filter(|nested| *nested <= MAX_LIST_DEPTH)
                    .ok_or(Error::Corrupt("row record list nests deeper than the supported depth"))?;
                for _ in 0..count {
                    self.skip_value(nested)?;
                }
            }
            TAG_NESTED_ROW => {
                let count = self.u32()? as usize;
                if count > self.remaining() {
                    return Err(Error::Corrupt("nested row length exceeds the remaining bytes"));
                }
                let nested = depth
                    .checked_add(1)
                    .filter(|nested| *nested <= MAX_LIST_DEPTH)
                    .ok_or(Error::Corrupt("nested row nests deeper than the supported depth"))?;
                for _ in 0..count {
                    let key_len = self.u32()? as usize;
                    self.take(key_len)?;
                    self.skip_value(nested)?;
                }
            }
            TAG_UNITE => {
                // schema id, layout revision, discriminant, payload member count.
                self.take(8 + 4 + 8 + 2)?;
                let count = self.u16()? as usize;
                let nested = depth
                    .checked_add(1)
                    .filter(|nested| *nested <= MAX_LIST_DEPTH)
                    .ok_or(Error::Corrupt("unite payload nests deeper than the supported depth"))?;
                for _ in 0..count {
                    self.skip_value(nested)?;
                }
            }
            TAG_STRUCTURE => {
                // schema id, layout revision, field count, member identities, bitmap.
                self.take(8 + 4 + 2)?;
                let count = self.u16()? as usize;
                self.take(count * 8)?;
                let bitmap = self.take(count.div_ceil(8))?;
                let present = (0..count).filter(|index| bitmap[index / 8] & (1 << (index % 8)) == 0).count();
                let nested = depth
                    .checked_add(1)
                    .filter(|nested| *nested <= MAX_LIST_DEPTH)
                    .ok_or(Error::Corrupt("structure nests deeper than the supported depth"))?;
                for _ in 0..present {
                    self.skip_value(nested)?;
                }
            }
            TAG_NAMED_ROW => {
                // schema id, member count, member identities, then one value each.
                self.take(8 + 2)?;
                let count = self.u16()? as usize;
                self.take(count * 8)?;
                let nested = depth
                    .checked_add(1)
                    .filter(|nested| *nested <= MAX_LIST_DEPTH)
                    .ok_or(Error::Corrupt("named class nests deeper than the supported depth"))?;
                for _ in 0..count {
                    self.skip_value(nested)?;
                }
            }
            _ => return Err(Error::Corrupt("row record value tag is unknown")),
        }
        Ok(self.position)
    }

    /// Advance past one length-prefixed UTF-8 string without materializing it.
    fn skip_text(&mut self) -> Result<()> {
        let len = self.u32()? as usize;
        self.take(len)?;
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }
}

fn decode_value(
    cursor: &mut RowCursor<'_>,
    references: &mut Option<RowReferences>,
    resolve: bool,
    revision_layout: Option<&RevisionLayout>,
    depth: u8,
) -> Result<Value> {
    Ok(match cursor.u8()? {
        TAG_NULL => Value::Null,
        TAG_BOOL => match cursor.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(Error::Corrupt("row record boolean is not 0 or 1")),
        },
        TAG_I64 => Value::I64(cursor.i64()?),
        TAG_U64 => Value::U64(cursor.u64()?),
        TAG_F64 => {
            let real = f64::from_le_bytes(cursor.take(8)?.try_into().unwrap());
            if !real.is_finite() {
                return Err(Error::Corrupt("row record real is not finite"));
            }
            Value::F64(real)
        }
        TAG_TEXT => Value::Text(cursor.text()?),
        TAG_UUID => Value::Uuid(uuid::Uuid::from_bytes(cursor.take(16)?.try_into().unwrap())),
        TAG_DATE => Value::Date(Date::from_days(cursor.i32()?)?),
        TAG_TIME => Value::Time(Time::from_nanos(cursor.i64()?)?),
        TAG_TIMESTAMP => Value::Timestamp(Timestamp::from_unix_micros(cursor.i64()?)?),
        TAG_DECIMAL => {
            let unscaled = cursor.i128()?;
            let precision = cursor.u8()?;
            let scale = cursor.u8()?;
            Value::Decimal(Decimal::new(unscaled, precision, scale)?)
        }
        TAG_BYTES => {
            let len = cursor.u32()? as usize;
            Value::Bytes(cursor.take(len)?.to_vec())
        }
        TAG_VECTOR => {
            let dimension = cursor.u16()? as usize;
            let payload_len = cursor.u32()? as usize;
            if dimension.saturating_mul(4) != payload_len {
                return Err(Error::Scalar { kind: "vector", message: "vector payload length disagrees with its declared dimension".into() });
            }
            let payload = cursor.take(payload_len)?;
            let mut components = Vec::with_capacity(dimension);
            for index in 0..dimension {
                let offset = index * 4;
                let component = f32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap());
                if !component.is_finite() {
                    return Err(Error::Scalar { kind: "vector", message: format!("component {index} is not finite") });
                }
                components.push(component);
            }
            Value::Vector(Vector::new(components).map_err(|error| Error::Scalar { kind: "vector", message: error.to_string() })?)
        }
        TAG_OBJECT => {
            let algo = algo_from_tag(cursor.u8()?)?;
            let kind = object_kind_from_tag(cursor.u8()?)?;
            let hash = cursor.digest()?;
            let size = cursor.u64()?;
            let object = ObjectRef { algo, hash, size, kind };
            if let Some(references) = references.as_mut() {
                references.objects.push(object.clone());
            }
            if resolve { resolve_object_reference(Value::Object(object))? } else { Value::Object(object) }
        }
        TAG_FILE => {
            let digest = digest_hex(&cursor.digest()?);
            let octets = cursor.u64()?;
            let flags = cursor.u8()?;
            if flags & !0b11 != 0 {
                return Err(Error::Corrupt("row record file flags are invalid"));
            }
            let name = if flags & 0b01 != 0 { Some(cursor.text()?) } else { None };
            let media_type = if flags & 0b10 != 0 { Some(cursor.text()?) } else { None };
            let hash = cursor.digest()?;
            let size = cursor.u64()?;
            let descriptor = FileDescriptor { digest, octets, name, media_type, manifest: FileManifestRef { hash, size } };
            if let Some(references) = references.as_mut() {
                references.files.push(descriptor.clone());
            }
            Value::File(descriptor)
        }
        TAG_LIST => {
            let count = cursor.u32()? as usize;
            // Every element takes at least one tag byte, so a count larger than
            // the remaining bytes cannot be a valid record.
            if count > cursor.remaining() {
                return Err(Error::Corrupt("row record list length exceeds the remaining bytes"));
            }
            let nested = depth
                .checked_add(1)
                .filter(|nested| *nested <= MAX_LIST_DEPTH)
                .ok_or(Error::Corrupt("row record list nests deeper than the supported depth"))?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_value(cursor, references, resolve, revision_layout, nested)?);
            }
            Value::List(items)
        }
        TAG_NESTED_ROW => {
            let count = cursor.u32()? as usize;
            if count > cursor.remaining() {
                return Err(Error::Corrupt("nested row length exceeds the remaining bytes"));
            }
            let nested = depth
                .checked_add(1)
                .filter(|nested| *nested <= MAX_LIST_DEPTH)
                .ok_or(Error::Corrupt("nested row nests deeper than the supported depth"))?;
            let mut fields = std::collections::BTreeMap::new();
            for _ in 0..count {
                let name = cursor.text()?;
                let value = decode_value(cursor, references, resolve, revision_layout, nested)?;
                fields.insert(name, value);
            }
            Value::Row(fields)
        }
        TAG_UNITE => decode_unite(cursor, references, resolve, revision_layout, depth)?,
        TAG_STRUCTURE => decode_structure(cursor, references, resolve, revision_layout, depth)?,
        TAG_NAMED_ROW => decode_named_row(cursor, references, resolve, revision_layout, depth)?,
        _ => return Err(Error::Corrupt("row record value tag is unknown")),
    })
}

/// Decodes one closed `structure` value back into a member-named record.
///
/// The record names its schema id, layout revision and durable member identities;
/// the directory supplies display names. Field count, identity order, bitmap
/// length and revision are all checked before any value is read, so a truncated
/// record or an unknown revision is a structured rejection.
fn decode_structure(
    cursor: &mut RowCursor<'_>,
    references: &mut Option<RowReferences>,
    resolve: bool,
    revision_layout: Option<&RevisionLayout>,
    depth: u8,
) -> Result<Value> {
    let schema_id = cursor.u64()?;
    let revision = cursor.u32()?;
    let field_count = cursor.u16()? as usize;
    let identities = cursor.take(field_count * 8)?;
    let bitmap_len = field_count.div_ceil(8);
    let bitmap = cursor.take(bitmap_len)?;
    let Some(layout) = revision_layout
    else {
        return Err(Error::Unsupported("decoding a `structure` value needs the schema directory installed for the statement"));
    };
    let named = layout
        .named(schema_id)
        .ok_or_else(|| Error::Corrupt("row record names a `structure` schema id that the directory does not declare"))?;
    let NamedValueLayout::Structure { revision: expected, fields: declared, .. } = named
    else {
        return Err(Error::Corrupt("row record names a schema id that is not a `structure` declaration"));
    };
    if *expected != revision {
        return Err(Error::Corrupt("row record uses a `structure` layout revision the directory does not declare"));
    }
    let mut ordered: Vec<&NamedFieldLayout> = declared.iter().collect();
    ordered.sort_by_key(|field| field.field_id);
    if ordered.len() != field_count {
        return Err(Error::Corrupt("row record `structure` field count disagrees with its declaration"));
    }
    for (index, field) in ordered.iter().enumerate() {
        let stored = u64::from_le_bytes(identities[index * 8..index * 8 + 8].try_into().unwrap());
        if stored != field.field_id {
            return Err(Error::Corrupt("row record `structure` member identity disagrees with its declaration"));
        }
    }
    let nested = depth
        .checked_add(1)
        .filter(|nested| *nested <= MAX_LIST_DEPTH)
        .ok_or(Error::Corrupt("structure nests deeper than the supported depth"))?;
    let mut fields = BTreeMap::new();
    for (index, field) in ordered.iter().enumerate() {
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            fields.insert(field.name.clone(), Value::Null);
            continue;
        }
        let value = decode_value(cursor, references, resolve, revision_layout, nested)?;
        fields.insert(field.name.clone(), value);
    }
    Ok(Value::Row(fields))
}

/// Decodes one `unite` value and re-establishes its canonical in-memory form.
///
/// The record names its schema id, layout revision, discriminant and payload
/// member identities. A revision change, an undeclared discriminant, an
/// identity-order mismatch or a payload count that disagrees with the variant is
/// a structured rejection: stored bytes are never reinterpreted under another
/// variant.
fn decode_unite(
    cursor: &mut RowCursor<'_>,
    references: &mut Option<RowReferences>,
    resolve: bool,
    revision_layout: Option<&RevisionLayout>,
    depth: u8,
) -> Result<Value> {
    let schema_id = cursor.u64()?;
    let revision = cursor.u32()?;
    let discriminant = cursor.i64()?;
    let member_count = cursor.u16()? as usize;
    let Some(layout) = revision_layout
    else {
        return Err(Error::Unsupported("decoding a `unite` value needs the schema directory installed for the statement"));
    };
    let named = layout
        .named(schema_id)
        .ok_or_else(|| Error::Corrupt("row record names a `unite` schema id that the directory does not declare"))?;
    let NamedValueLayout::Unite { revision: expected, variants, .. } = named
    else {
        return Err(Error::Corrupt("row record names a schema id that is not a `unite` declaration"));
    };
    if *expected != revision {
        return Err(Error::Corrupt("row record uses a `unite` layout revision the directory does not declare"));
    }
    let variant = variants
        .iter()
        .find(|variant| variant.discriminant == discriminant)
        .ok_or_else(|| Error::Corrupt("row record names a `unite` variant the declaration does not contain"))?;
    if variant.fields.len() != member_count {
        return Err(Error::Corrupt("row record `unite` payload member count disagrees with its variant"));
    }
    let nested = depth
        .checked_add(1)
        .filter(|nested| *nested <= MAX_LIST_DEPTH)
        .ok_or(Error::Corrupt("unite payload nests deeper than the supported depth"))?;
    let mut payload = BTreeMap::new();
    for field in &variant.fields {
        let value = decode_value(cursor, references, resolve, revision_layout, nested)?;
        payload.insert(field.name.clone(), value);
    }
    Ok(Value::List(vec![Value::I64(discriminant), Value::Row(payload)]))
}

/// Decodes one named `class` value back into a member-named record.
///
/// Members are identified by durable field identity, so a renamed or reordered
/// declaration still binds the stored values to the right names.
fn decode_named_row(
    cursor: &mut RowCursor<'_>,
    references: &mut Option<RowReferences>,
    resolve: bool,
    revision_layout: Option<&RevisionLayout>,
    depth: u8,
) -> Result<Value> {
    let schema_id = cursor.u64()?;
    let member_count = cursor.u16()? as usize;
    let identities = cursor.take(member_count * 8)?;
    let Some(layout) = revision_layout
    else {
        return Err(Error::Unsupported("decoding a named `class` value needs the schema directory installed for the statement"));
    };
    let named = layout
        .named(schema_id)
        .ok_or_else(|| Error::Corrupt("row record names a `class` schema id that the directory does not declare"))?;
    let NamedValueLayout::Class { fields: declared, .. } = named
    else {
        return Err(Error::Corrupt("row record names a schema id that is not a `class` declaration"));
    };
    let nested = depth
        .checked_add(1)
        .filter(|nested| *nested <= MAX_LIST_DEPTH)
        .ok_or(Error::Corrupt("named class nests deeper than the supported depth"))?;
    let mut fields = BTreeMap::new();
    for index in 0..member_count {
        let field_id = u64::from_le_bytes(identities[index * 8..index * 8 + 8].try_into().unwrap());
        let declared = declared
            .iter()
            .find(|field| field.field_id == field_id)
            .ok_or_else(|| Error::Corrupt("row record names a `class` member the declaration does not contain"))?;
        let value = decode_value(cursor, references, resolve, revision_layout, nested)?;
        fields.insert(declared.name.clone(), value);
    }
    Ok(Value::Row(fields))
}

fn resolve_object_reference(value: Value) -> Result<Value> {
    match value {
        Value::Object(object) => OBJECT_RESOLVER.with(|slot| match slot.borrow().as_ref() {
            Some(resolver) => {
                let bytes = resolver.resolve(&object)?;
                match object.kind {
                    ObjectKind::VectorPayload => Vector::from_le_bytes(&bytes).map(Value::Vector),
                    _ => Ok(Value::Bytes(bytes)),
                }
            }
            None => Err(Error::Unsupported("a stored object reference needs an object store to decode")),
        }),
        other => Ok(other),
    }
}
