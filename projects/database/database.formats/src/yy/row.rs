//! `.yydb` 记录值中的表行记录编解码。
//!
//! 一行表记录以 `__yydb/v0/row/<table>/<pk>` 为记录名，值采用带版本前缀的二进
//! 制行格式：版本字节、列数、NULL 位图、按列名升序的名字表和逐个带标签的值。
//! 旧 JSON 行格式与 v1 二进制行格式（字段长度 16 位）均已退役，解码遇到非当前
//! 版本一律失败并提示重建，不猜测形状。
//!
//! 行内的 `bytes` 与 `vector<N>` 可能被宿主改写为对象引用；解码需要
//! [`ObjectResolver`]，由宿主在一条语句执行期间用 [`with_object_resolver`] 安装。

use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::Arc,
};

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
/// not [`ROW_CODEC_VERSION`] was written by a retired codec (the JSON encoding
/// or the v1 binary encoding, which capped every field at 65535 bytes), and
/// decoding it fails closed with a rebuild hint instead of guessing a shape.
pub const ROW_CODEC_VERSION: u8 = 2;

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
/// Layout: `version u8`, `column count u16`, `null bitmap ceil(n/8)`, then one
/// `name length u32 + UTF-8 name` per column in ascending name order, then one
/// tagged value for every column the bitmap does not mark null. Integers keep
/// their declared width, text and octets carry a length prefix, floats are raw
/// IEEE-754 bits, and keys never repeat the enclosing table name.
pub fn encode_row(row: &BTreeMap<String, Value>) -> Result<Vec<u8>> {
    let column_count = u16::try_from(row.len()).map_err(|_| Error::Unsupported("query row has more than 65535 columns"))?;
    let mut out = Vec::with_capacity(4 + row.len() * 12);
    out.push(ROW_CODEC_VERSION);
    out.extend_from_slice(&column_count.to_le_bytes());
    let bitmap_len = row.len().div_ceil(8);
    let bitmap_start = out.len();
    out.resize(bitmap_start + bitmap_len, 0);
    for (index, (name, value)) in row.iter().enumerate() {
        if matches!(value, Value::Null) {
            out[bitmap_start + index / 8] |= 1 << (index % 8);
        }
        put_len(&mut out, name.len(), "column name")?;
        out.extend_from_slice(name.as_bytes());
    }
    for value in row.values() {
        if matches!(value, Value::Null) {
            continue;
        }
        encode_value(&mut out, value, 0)?;
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
    let mut selected = vec![None; names.len()];
    for (slot, name) in names.iter().enumerate() {
        let Some(value) = row.get(*name) else {
            continue;
        };
        if matches!(value, Value::Null) {
            continue;
        }
        let mut encoded = Vec::new();
        encode_value(&mut encoded, value, 0)?;
        selected[slot] = Some(encoded);
    }
    Ok(selected)
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
    let mut cursor = RowCursor::new(bytes);
    cursor.take(1)?;
    let column_count = cursor.u16()? as usize;
    let bitmap = cursor.take(column_count.div_ceil(8))?;
    // Two cursors over one record: one reads the name table, the other the values
    // that follow it, so a skipped value is never decoded.
    let names_start = cursor;
    let mut names_cursor = names_start;
    for _ in 0..column_count {
        names_cursor.skip_text()?;
    }
    let mut values = names_cursor;
    let mut names_cursor = names_start;
    for index in 0..column_count {
        let name = names_cursor.text_slice()?;
        // A NULL column carries no value bytes: the row bitmap already said so.
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            continue;
        }
        let start = values.position;
        let end = values.skip_value(0)?;
        if let Some(slot) = names.iter().position(|wanted| *wanted == name) {
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

fn encode_value(out: &mut Vec<u8>, value: &Value, depth: u8) -> Result<()> {
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
        Value::List(items) => {
            out.push(TAG_LIST);
            let count = u32::try_from(items.len()).map_err(|_| Error::Unsupported("row record list has more than 4294967295 elements"))?;
            out.extend_from_slice(&count.to_le_bytes());
            let nested = depth
                .checked_add(1)
                .filter(|nested| *nested <= MAX_LIST_DEPTH)
                .ok_or(Error::Unsupported("row record list nests deeper than the supported depth"))?;
            for item in items {
                encode_value(out, item, nested)?;
            }
        }
        _ => {
            return Err(Error::Unsupported(
                "query row encoding supports null, bool, i64, real, text, uuid, bytes, vector, date, time, timestamp, \
                 decimal, bytes object references, file descriptors, and lists only",
            ));
        }
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
    let column_count = header.u16()? as usize;
    let bitmap_len = column_count.div_ceil(8);
    // The bitmap stays a slice of the record; copying it would add one heap
    // allocation per decoded row for no benefit.
    let bitmap = header.take(bitmap_len)?;
    // Walk the name table once to find where the values start instead of
    // collecting the names into a `Vec`: the names are only needed one at a
    // time, and each decoded name is moved straight into the row as its key.
    let names_start = header;
    let mut walk = names_start;
    for _ in 0..column_count {
        walk.skip_text()?;
    }
    let mut names = names_start;
    let mut values = walk;
    let mut row = BTreeMap::new();
    for index in 0..column_count {
        let name = names.text()?;
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            row.insert(name, Value::Null);
            continue;
        }
        let value = decode_value(&mut values, references, resolve, 0)?;
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
            TAG_I64 | TAG_F64 | TAG_TIME | TAG_TIMESTAMP => {
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

fn decode_value(cursor: &mut RowCursor<'_>, references: &mut Option<RowReferences>, resolve: bool, depth: u8) -> Result<Value> {
    Ok(match cursor.u8()? {
        TAG_NULL => Value::Null,
        TAG_BOOL => match cursor.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(Error::Corrupt("row record boolean is not 0 or 1")),
        },
        TAG_I64 => Value::I64(cursor.i64()?),
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
                items.push(decode_value(cursor, references, resolve, nested)?);
            }
            Value::List(items)
        }
        _ => return Err(Error::Corrupt("row record value tag is unknown")),
    })
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
