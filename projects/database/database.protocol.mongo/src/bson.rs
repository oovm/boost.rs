//! 足以支撑 MongoDB OP_MSG 线处理的 BSON 文档序列化 / 解析。
//!
//! 覆盖 BSON 规范的全部元素类型：Document (0x03)、Array (0x04)、String (0x02)、
//! Int32 (0x10)、Int64 (0x12)、Double (0x01)、Decimal128 (0x13)、Bool (0x08)、
//! Null (0x0A)、Undefined (0x06)、ObjectId (0x07)、Binary (0x05，保留子类型)、
//! Date (0x09)、Timestamp (0x11)、Regex (0x0B)、Code (0x0D)、CodeWScope (0x0F)、
//! DbPointer (0x0C)、Symbol (0x0E)、MinKey (0xFF) 与 MaxKey (0x7F)。
//!
//! `mongod 8.0.9` 会把这些类型原样回显在 `find` / `explain` 应答里，缺任何一种
//! 都会让真机应答整条不可解码。

use std::{cmp::Ordering, io};

use crate::decimal128::Decimal128;

/// 带子类型的二进制载荷。
///
/// `bytes` 只存线上载荷本身，不含外层长度与子类型字节。已废弃的子类型
/// `0x02` 在线上还会内嵌一份自身长度，解码时剥离、编码时还原，因此往返
/// 字节完全一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryData {
    /// BSON 二进制子类型（`0x00`、`0x02`、`0x05`、`0x80` 等）。
    pub subtype: u8,
    /// 线上载荷字节，不含外层长度与子类型。
    pub bytes: Vec<u8>,
}

impl BinaryData {
    /// 组装一份二进制载荷。
    pub fn new(subtype: u8, bytes: Vec<u8>) -> Self {
        Self { subtype, bytes }
    }
}

/// 本协议栈支持的 BSON 值变体。
#[derive(Debug, Clone, PartialEq)]
pub enum BsonValue {
    /// Embedded document (0x03).
    Document(BsonDocument),
    /// Array (0x04).
    Array(Vec<BsonValue>),
    /// UTF-8 string (0x02).
    String(String),
    /// 32-bit signed integer (0x10).
    Int32(i32),
    /// 64-bit signed integer (0x12).
    Int64(i64),
    /// IEEE-754 double (0x01).
    Double(f64),
    /// Boolean (0x08).
    Bool(bool),
    /// Null placeholder (0x0A).
    Null,
    /// 已废弃的 BSON undefined (0x06)。
    ///
    /// `mongod` 对多键索引上空数组的索引键会写成 undefined，因此唯一索引
    /// 冲突应答里的 `keyValue: { a: undefined }` 必须能原样复现。
    Undefined,
    /// 12-byte ObjectId (0x07).
    ObjectId([u8; 12]),
    /// Binary payload (0x05) 连同其子类型。
    Binary(BinaryData),
    /// BSON Date (0x09)：自 Unix 纪元起的毫秒数，始终按 UTC 解释。
    DateTime(i64),
    /// BSON Timestamp (0x11)：秒级时间戳 `t` 与自增计数 `i`。
    ///
    /// 线上字节先写 `i` 再写 `t`，但排序与比较按 `(t, i)` 字典序进行。
    Timestamp {
        /// 自 Unix 纪元起的秒数。
        t: u32,
        /// 同一秒内的自增计数。
        i: u32,
    },
    /// Decimal128 (0x13)。
    Decimal128(Decimal128),
    /// 正则表达式 (0x0B)：模式与选项分开存储，选项按 `mongod` 的字母序回显。
    Regex {
        /// 正则模式，不含分隔符。
        pattern: String,
        /// 选项字母（`i`、`m`、`s`、`x` 等）。
        options: String,
    },
    /// JavaScript 代码 (0x0D)。
    Code(String),
    /// 带作用域的 JavaScript 代码 (0x0F)。
    CodeWithScope {
        /// 代码文本。
        code: String,
        /// 代码作用域文档。
        scope: BsonDocument,
    },
    /// 已废弃的 DBPointer (0x0C)：命名空间与目标 ObjectId。
    DbPointer {
        /// 目标命名空间 `<database>.<collection>`。
        namespace: String,
        /// 目标文档的 ObjectId。
        id: [u8; 12],
    },
    /// 已废弃的 Symbol (0x0E)，文本语义与 String 相同。
    Symbol(String),
    /// MinKey (0xFF)：小于任何其他值。
    MinKey,
    /// MaxKey (0x7F)：大于任何其他值。
    MaxKey,
}

impl BsonValue {
    /// BSON 规范里的类型排序位次。
    ///
    /// 顺序取自 `mongod 8.0.9` 的 `sort` 结果：`MinKey` 最小、`MaxKey` 最大，
    /// `Date` 排在 `Bool` 之后、`Timestamp` 之前，`Regex` / `Code` / `CodeWScope`
    /// 依次排在其后。`Symbol` 与 `String` 同位次并按文本比较。
    fn type_rank(&self) -> u8 {
        match self {
            Self::MinKey => 0,
            Self::Undefined => 1,
            Self::Null => 2,
            Self::Double(_) | Self::Int32(_) | Self::Int64(_) | Self::Decimal128(_) => 3,
            Self::String(_) | Self::Symbol(_) => 4,
            Self::Document(_) => 5,
            Self::Array(_) => 6,
            Self::Binary(_) => 7,
            Self::ObjectId(_) => 8,
            Self::Bool(_) => 9,
            Self::DateTime(_) => 10,
            Self::Timestamp { .. } => 11,
            Self::Regex { .. } => 12,
            // DBPointer 已废弃且真机排序位次无法用现代驱动构造语料，这里固定在
            // Regex 与 Code 之间，保证比较是全序。
            Self::DbPointer { .. } => 13,
            Self::Code(_) => 14,
            Self::CodeWithScope { .. } => 15,
            Self::MaxKey => 16,
        }
    }

    /// BSON 规范比较顺序。
    ///
    /// `MinKey < 一切 < MaxKey`；`Date` 按毫秒比较；`Timestamp` 按 `(t, i)` 比较；
    /// 各数值类型共用一个数值空间；`Symbol` 与同文本的 `String` 相等。该顺序
    /// 直接决定索引键与 `sort` 的结果。
    pub fn compare(&self, other: &BsonValue) -> Ordering {
        let left_rank = self.type_rank();
        let right_rank = other.type_rank();
        if left_rank != right_rank {
            return left_rank.cmp(&right_rank);
        }
        match (self, other) {
            (Self::Double(_) | Self::Int32(_) | Self::Int64(_) | Self::Decimal128(_), _) => compare_numbers(self, other),
            (Self::String(left), Self::String(right)) | (Self::Symbol(left), Self::Symbol(right)) => left.cmp(right),
            (Self::String(left), Self::Symbol(right)) | (Self::Symbol(left), Self::String(right)) => left.cmp(right),
            (Self::Document(left), Self::Document(right)) => compare_documents(left, right),
            (Self::Array(left), Self::Array(right)) => {
                for (left, right) in left.iter().zip(right.iter()) {
                    let ordering = left.compare(right);
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                left.len().cmp(&right.len())
            }
            (Self::Binary(left), Self::Binary(right)) => {
                (left.bytes.len(), left.subtype, &left.bytes).cmp(&(right.bytes.len(), right.subtype, &right.bytes))
            }
            (Self::ObjectId(left), Self::ObjectId(right)) => left.cmp(right),
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (Self::DateTime(left), Self::DateTime(right)) => left.cmp(right),
            (Self::Timestamp { t: left_t, i: left_i }, Self::Timestamp { t: right_t, i: right_i }) => {
                left_t.cmp(right_t).then(left_i.cmp(right_i))
            }
            (Self::Regex { pattern: left_pattern, options: left_options }, Self::Regex { pattern: right_pattern, options: right_options }) => {
                left_pattern.cmp(right_pattern).then(left_options.cmp(right_options))
            }
            (Self::Code(left), Self::Code(right)) => left.cmp(right),
            (Self::CodeWithScope { code: left_code, scope: left_scope }, Self::CodeWithScope { code: right_code, scope: right_scope }) => {
                left_code.cmp(right_code).then_with(|| compare_documents(left_scope, right_scope))
            }
            (Self::DbPointer { namespace: left, id: left_id }, Self::DbPointer { namespace: right, id: right_id }) => {
                left.cmp(right).then(left_id.cmp(right_id))
            }
            _ => Ordering::Equal,
        }
    }
}

impl PartialOrd for BsonValue {
    /// 始终返回 [`BsonValue::compare`] 的结果，`NaN` 与跨类型值也有确定位次。
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.compare(other))
    }
}

/// 同一数值空间内的比较：整数对整数精确比较，其余按 `f64`，decimal128 内部
/// 按十进制精确比较。
fn compare_numbers(left: &BsonValue, right: &BsonValue) -> Ordering {
    match (left, right) {
        (BsonValue::Decimal128(left), BsonValue::Decimal128(right)) => left.compare(right),
        (BsonValue::Decimal128(left), _) => compare_f64(left.as_f64(), number_to_f64(right)),
        (_, BsonValue::Decimal128(right)) => compare_f64(number_to_f64(left), right.as_f64()),
        _ => match (integral_of(left), integral_of(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => compare_f64(number_to_f64(left), number_to_f64(right)),
        },
    }
}

/// 数值变体的 `f64` 近似；非数值返回 `NaN`。
fn number_to_f64(value: &BsonValue) -> f64 {
    match value {
        BsonValue::Int32(number) => *number as f64,
        BsonValue::Int64(number) => *number as f64,
        BsonValue::Double(number) => *number,
        BsonValue::Decimal128(number) => number.as_f64(),
        _ => f64::NAN,
    }
}

/// 精确整数视图，供整数与整数比较使用。
fn integral_of(value: &BsonValue) -> Option<i64> {
    match value {
        BsonValue::Int32(number) => Some(*number as i64),
        BsonValue::Int64(number) => Some(*number),
        BsonValue::Double(number) => integral_value(*number),
        BsonValue::Decimal128(number) => number.as_i64(),
        _ => None,
    }
}

/// `NaN` 排在所有数值之前，与 `mongod` 的排序一致。
fn compare_f64(left: f64, right: f64) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

/// 文档比较：先比字段名，再比字段值，最后比字段数量。
fn compare_documents(left: &BsonDocument, right: &BsonDocument) -> Ordering {
    for ((left_key, left_value), (right_key, right_value)) in left.fields.iter().zip(right.fields.iter()) {
        let ordering = left_key.cmp(right_key).then_with(|| left_value.compare(right_value));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.fields.len().cmp(&right.fields.len())
}

/// A BSON document preserves field insertion order (unlike a HashMap) because BSON
/// itself is an ordered key/value mapping.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BsonDocument {
    /// Ordered key/value pairs. `pub(crate)` so the gateway can iterate over inserts
    /// and build writeResponses.
    pub fields: Vec<(String, BsonValue)>,
}

impl BsonDocument {
    /// Empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one key/value pair, preserving insertion order.
    pub fn push(&mut self, key: impl Into<String>, value: BsonValue) {
        self.fields.push((key.into(), value));
    }

    /// Ordered key/value pairs as written on the wire.
    pub fn fields(&self) -> &[(String, BsonValue)] {
        &self.fields
    }

    /// First value under `key`, if present.
    pub fn get(&self, key: &str) -> Option<&BsonValue> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Convenience getter for command names — returns the first key whose value is any
    /// type (we only need the key name for dispatch).
    pub fn command_name(&self) -> Option<&str> {
        self.fields.first().map(|(k, _)| k.as_str())
    }

    /// Extract the `$db` field as a UTF-8 string, defaulting to `"admin"`.
    pub fn database(&self) -> String {
        match self.get("$db") {
            Some(BsonValue::String(db)) => db.clone(),
            _ => "admin".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn write_cstring(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as i32 + 1).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// 数组下标键直接写数字，不分摊一次 `String` 分配。
fn write_index_key(out: &mut Vec<u8>, index: usize) {
    let mut digits = [0_u8; 20];
    let mut position = digits.len();
    let mut remaining = index;
    loop {
        position -= 1;
        digits[position] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[position..]);
    out.push(0);
}

/// 数组下标键的十进制位数，供 [`BsonValue::encoded_len`] 精确计数。
fn index_key_len(index: usize) -> usize {
    index.checked_ilog10().map_or(1, |digits| digits as usize + 1)
}

/// Writes one document into `out` and patches its length prefix in place.
///
/// 整份文档（含嵌套文档与数组）只追加进调用方给的同一个缓冲区，因此编码一份
/// 文档不再为每个子文档/数组分配临时 `Vec`。
fn encode_document(out: &mut Vec<u8>, document: &BsonDocument) {
    let start = out.len();
    out.extend_from_slice(&0_u32.to_le_bytes());
    for (key, value) in &document.fields {
        encode_field(out, key, value);
    }
    out.push(0); // terminator
    let length = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&length.to_le_bytes());
}

fn encode_field(out: &mut Vec<u8>, key: &str, value: &BsonValue) {
    out.push(element_type(value));
    write_cstring(out, key);
    encode_payload(out, value);
}

fn encode_indexed_field(out: &mut Vec<u8>, index: usize, value: &BsonValue) {
    out.push(element_type(value));
    write_index_key(out, index);
    encode_payload(out, value);
}

/// BSON element type byte of one value.
fn element_type(value: &BsonValue) -> u8 {
    match value {
        BsonValue::Double(_) => 0x01,
        BsonValue::String(_) => 0x02,
        BsonValue::Document(_) => 0x03,
        BsonValue::Array(_) => 0x04,
        BsonValue::Binary(_) => 0x05,
        BsonValue::Undefined => 0x06,
        BsonValue::ObjectId(_) => 0x07,
        BsonValue::Bool(_) => 0x08,
        BsonValue::DateTime(_) => 0x09,
        BsonValue::Null => 0x0A,
        BsonValue::Regex { .. } => 0x0B,
        BsonValue::DbPointer { .. } => 0x0C,
        BsonValue::Code(_) => 0x0D,
        BsonValue::Symbol(_) => 0x0E,
        BsonValue::CodeWithScope { .. } => 0x0F,
        BsonValue::Int32(_) => 0x10,
        BsonValue::Timestamp { .. } => 0x11,
        BsonValue::Int64(_) => 0x12,
        BsonValue::Decimal128(_) => 0x13,
        BsonValue::MaxKey => 0x7F,
        BsonValue::MinKey => 0xFF,
    }
}

/// Writes the value payload that follows the element type and key.
fn encode_payload(out: &mut Vec<u8>, value: &BsonValue) {
    match value {
        BsonValue::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
        BsonValue::String(v) => write_string(out, v),
        BsonValue::Document(document) => encode_document(out, document),
        BsonValue::Array(items) => {
            let start = out.len();
            out.extend_from_slice(&0_u32.to_le_bytes());
            for (index, item) in items.iter().enumerate() {
                encode_indexed_field(out, index, item);
            }
            out.push(0);
            let length = (out.len() - start) as u32;
            out[start..start + 4].copy_from_slice(&length.to_le_bytes());
        }
        BsonValue::Binary(v) => {
            // BSON 规范：子类型 0x02（已废弃的旧式二进制）在载荷前再内嵌一份
            // 自身长度。存储侧只保留载荷，这里按长度还原。
            let wire_len = if v.subtype == 0x02 { v.bytes.len() + 4 } else { v.bytes.len() };
            out.extend_from_slice(&(wire_len as i32).to_le_bytes());
            out.push(v.subtype);
            if v.subtype == 0x02 {
                out.extend_from_slice(&(v.bytes.len() as i32).to_le_bytes());
            }
            out.extend_from_slice(&v.bytes);
        }
        BsonValue::Undefined | BsonValue::Null | BsonValue::MinKey | BsonValue::MaxKey => {}
        BsonValue::ObjectId(bytes) => out.extend_from_slice(bytes),
        BsonValue::Bool(v) => out.push(if *v { 1 } else { 0 }),
        BsonValue::Int32(v) => out.extend_from_slice(&v.to_le_bytes()),
        BsonValue::Int64(v) => out.extend_from_slice(&v.to_le_bytes()),
        BsonValue::DateTime(v) => out.extend_from_slice(&v.to_le_bytes()),
        BsonValue::Timestamp { t, i } => {
            // 线上顺序是先 `i` 后 `t`。
            out.extend_from_slice(&i.to_le_bytes());
            out.extend_from_slice(&t.to_le_bytes());
        }
        BsonValue::Decimal128(v) => out.extend_from_slice(&v.bytes()),
        BsonValue::Regex { pattern, options } => {
            write_cstring(out, pattern);
            write_cstring(out, options);
        }
        BsonValue::Code(code) => write_string(out, code),
        BsonValue::CodeWithScope { code, scope } => {
            // 总长度包含自身 4 字节，后面紧跟代码字符串与作用域文档。
            let start = out.len();
            out.extend_from_slice(&0_u32.to_le_bytes());
            write_string(out, code);
            encode_document(out, scope);
            let length = (out.len() - start) as u32;
            out[start..start + 4].copy_from_slice(&length.to_le_bytes());
        }
        BsonValue::DbPointer { namespace, id } => {
            write_string(out, namespace);
            out.extend_from_slice(id);
        }
        BsonValue::Symbol(text) => write_string(out, text),
    }
}

/// Encodes a [`BsonDocument`] into a freshly allocated byte vector.
pub fn encode_bson(doc: &BsonDocument) -> Vec<u8> {
    let mut out = Vec::new();
    encode_document(&mut out, doc);
    out
}

impl BsonDocument {
    /// Number of bytes [`encode_bson`] writes for this document.
    ///
    /// 供分页与体积上限判断使用：只做一次算术遍，不产生编码缓冲区。
    pub fn encoded_len(&self) -> usize {
        let mut length = 5; // 4 字节长度前缀 + 终止符
        for (key, value) in &self.fields {
            length += 1 + key.len() + 1 + value.encoded_len();
        }
        length
    }
}

impl BsonValue {
    /// Number of bytes this value contributes after its type byte and key.
    pub fn encoded_len(&self) -> usize {
        match self {
            BsonValue::Double(_) | BsonValue::DateTime(_) | BsonValue::Int64(_) => 8,
            BsonValue::String(text) | BsonValue::Symbol(text) | BsonValue::Code(text) => 4 + text.len() + 1,
            BsonValue::Document(document) => document.encoded_len(),
            BsonValue::Array(items) => {
                let mut length = 5;
                for (index, item) in items.iter().enumerate() {
                    length += 1 + index_key_len(index) + 1 + item.encoded_len();
                }
                length
            }
            BsonValue::Binary(binary) => {
                let wire_len = if binary.subtype == 0x02 { binary.bytes.len() + 4 } else { binary.bytes.len() };
                4 + 1 + wire_len
            }
            BsonValue::Undefined | BsonValue::Null | BsonValue::MinKey | BsonValue::MaxKey => 0,
            BsonValue::ObjectId(_) => 12,
            BsonValue::Bool(_) => 1,
            BsonValue::Int32(_) => 4,
            BsonValue::Timestamp { .. } => 8,
            BsonValue::Decimal128(_) => 16,
            BsonValue::Regex { pattern, options } => pattern.len() + 1 + options.len() + 1,
            BsonValue::CodeWithScope { code, scope } => 4 + 4 + code.len() + 1 + scope.encoded_len(),
            BsonValue::DbPointer { namespace, .. } => 4 + namespace.len() + 1 + 12,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Errors that can occur while decoding BSON or the OP_MSG body that wraps it.
#[derive(Debug)]
pub enum BsonError {
    /// I/O failure reading bytes from the source buffer.
    Io(io::Error),
    /// The BSON document was malformed (wrong length, unknown type, etc.).
    Malformed(String),
    /// A field we need for command dispatch was missing or not the expected type.
    MissingField(String),
}

impl From<io::Error> for BsonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 解析过程中的读取位置。
///
/// 所有读取都在输入切片上按偏移推进，不再经过 `Cursor` + `read_exact`，因此
/// 解析一份文档不再逐字节发起 I/O 调用，嵌套文档也不再做一次整段拷贝。
struct SliceReader<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> SliceReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    fn read_u8(&mut self) -> Option<u8> {
        let byte = *self.input.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(count)?;
        let slice = self.input.get(self.position..end)?;
        self.position = end;
        Some(slice)
    }

    fn read_i32(&mut self) -> Option<i32> {
        let bytes = self.take(4)?;
        Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i64(&mut self) -> Option<i64> {
        let bytes = self.take(8)?;
        Some(i64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_f64(&mut self) -> Option<f64> {
        let bytes = self.take(8)?;
        Some(f64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Reads a NUL-terminated key, validating UTF-8 like the old byte-by-byte
    /// reader did.
    fn read_cstring(&mut self) -> Result<String, BsonError> {
        let rest = self.input.get(self.position..).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        let length = rest.iter().position(|byte| *byte == 0).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        let text = std::str::from_utf8(&rest[..length]).map_err(|_| BsonError::Malformed("invalid UTF-8 in cstring".into()))?;
        self.position += length + 1;
        Ok(text.to_string())
    }

    /// Same as [`Self::read_cstring`] but discards the text, so array keys cost
    /// a validation instead of an allocation.
    fn skip_cstring(&mut self) -> Result<(), BsonError> {
        let rest = self.input.get(self.position..).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        let length = rest.iter().position(|byte| *byte == 0).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        std::str::from_utf8(&rest[..length]).map_err(|_| BsonError::Malformed("invalid UTF-8 in cstring".into()))?;
        self.position += length + 1;
        Ok(())
    }

    /// Reads a NUL-terminated key as a raw slice, validating UTF-8 like
    /// [`Self::read_cstring`] but without allocating a `String`.
    fn read_raw_cstring(&mut self) -> Result<&'a [u8], BsonError> {
        let rest = self.input.get(self.position..).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        let length = rest.iter().position(|byte| *byte == 0).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        std::str::from_utf8(&rest[..length]).map_err(|_| BsonError::Malformed("invalid UTF-8 in cstring".into()))?;
        self.position += length + 1;
        Ok(&rest[..length])
    }

    fn read_string(&mut self) -> Result<String, BsonError> {
        let length = self.read_i32().ok_or_else(|| BsonError::Malformed("invalid BSON string length".into()))?;
        if length < 1 {
            return Err(BsonError::Malformed("invalid BSON string length".into()));
        }
        let bytes = self.take((length - 1) as usize).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        // 终止符与旧读取器一致：只消费，不校验取值。
        self.read_u8().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
        String::from_utf8(bytes.to_vec()).map_err(|_| BsonError::Malformed("invalid UTF-8 in BSON string".into()))
    }
}

/// Parses a BSON document from a byte slice.
pub fn parse_bson(input: &[u8]) -> Result<BsonDocument, BsonError> {
    let mut reader = SliceReader::new(input);
    let doc_len = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
    if doc_len < 0 {
        return Err(BsonError::Malformed("negative BSON document length".into()));
    }
    if doc_len < 5 {
        return Err(BsonError::Malformed("BSON document too short".into()));
    }
    if doc_len as usize > input.len() {
        return Err(BsonError::Malformed("BSON length exceeds input".into()));
    }
    let mut doc = BsonDocument::new();
    loop {
        let Some(elem_type) = reader.read_u8()
        else {
            return Err(BsonError::Malformed("unexpected end of BSON".into()));
        };
        if elem_type == 0 {
            break;
        }
        let key = reader.read_cstring()?;
        let value = parse_value(&mut reader, elem_type)?;
        doc.push(key, value);
    }
    Ok(doc)
}

/// Parses only the top-level `fields` of a BSON document.
///
/// 过滤谓词只读取它引用的顶层字段时，其余字段可以按线协议长度跳过，不必为
/// 整表扫描里的每一篇文档都建一份完整 `BsonDocument`。字段名按顶层匹配：
/// 需要 `a.b` 时调用方传入 `a`，整棵子树照常解码，嵌套路径语义不变。
pub fn parse_bson_projected(input: &[u8], fields: &[String]) -> Result<BsonDocument, BsonError> {
    let mut reader = SliceReader::new(input);
    let doc_len = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
    if doc_len < 0 {
        return Err(BsonError::Malformed("negative BSON document length".into()));
    }
    if doc_len < 5 {
        return Err(BsonError::Malformed("BSON document too short".into()));
    }
    if doc_len as usize > input.len() {
        return Err(BsonError::Malformed("BSON length exceeds input".into()));
    }
    let mut doc = BsonDocument::new();
    loop {
        let Some(elem_type) = reader.read_u8()
        else {
            return Err(BsonError::Malformed("unexpected end of BSON".into()));
        };
        if elem_type == 0 {
            break;
        }
        let key = reader.read_raw_cstring()?;
        if fields.iter().any(|field| field.as_bytes() == key) {
            let name = String::from_utf8(key.to_vec()).map_err(|_| BsonError::Malformed("invalid UTF-8 in cstring".into()))?;
            let value = parse_value(&mut reader, elem_type)?;
            doc.push(name, value);
        }
        else {
            skip_value(&mut reader, elem_type)?;
        }
    }
    Ok(doc)
}

/// Skips one BSON value without materialising it.
fn skip_value(reader: &mut SliceReader<'_>, elem_type: u8) -> Result<(), BsonError> {
    fn take(reader: &mut SliceReader<'_>, count: usize) -> Result<(), BsonError> {
        reader.take(count).map(|_| ()).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))
    }
    match elem_type {
        0x01 => take(reader, 8),
        // string / JavaScript code / symbol: i32 length including the NUL.
        0x02 | 0x0D | 0x0E => {
            let length = reader.read_i32().ok_or_else(|| BsonError::Malformed("invalid BSON string length".into()))?;
            if length < 1 {
                return Err(BsonError::Malformed("invalid BSON string length".into()));
            }
            take(reader, length as usize)
        }
        // document / array: i32 length including the trailing NUL.
        0x03 | 0x04 => {
            let length = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            if length < 5 {
                return Err(BsonError::Malformed("BSON sub-document too short".into()));
            }
            take(reader, (length - 4) as usize)
        }
        // binary: i32 length, subtype byte, payload.
        0x05 => {
            let length = reader.read_i32().ok_or_else(|| BsonError::Malformed("negative BSON binary length".into()))?;
            if length < 0 {
                return Err(BsonError::Malformed("negative BSON binary length".into()));
            }
            take(reader, 1 + length as usize)
        }
        0x06 | 0x7F | 0xFF | 0x0A => Ok(()),
        0x07 => take(reader, 12),
        0x08 => take(reader, 1),
        // date / timestamp / int64.
        0x09 | 0x11 | 0x12 => take(reader, 8),
        // regex: two cstrings.
        0x0B => {
            reader.skip_cstring()?;
            reader.skip_cstring()
        }
        // dbPointer: string plus 12-byte object id.
        0x0C => {
            let length = reader.read_i32().ok_or_else(|| BsonError::Malformed("invalid BSON string length".into()))?;
            if length < 1 {
                return Err(BsonError::Malformed("invalid BSON string length".into()));
            }
            take(reader, length as usize)?;
            take(reader, 12)
        }
        // code with scope: i32 total length including itself.
        0x0F => {
            let length = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            if length < 4 {
                return Err(BsonError::Malformed("BSON code with scope too short".into()));
            }
            take(reader, (length - 4) as usize)
        }
        0x10 => take(reader, 4),
        0x13 => take(reader, 16),
        other => Err(BsonError::Malformed(format!("unsupported BSON element type {other:#x}"))),
    }
}

/// Hands out the body of one length-prefixed sub-document.
fn bounded_sub_document<'a>(reader: &mut SliceReader<'a>) -> Result<SliceReader<'a>, BsonError> {
    let len = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
    if len < 0 {
        return Err(BsonError::Malformed("negative sub-document length".into()));
    }
    if len < 5 {
        return Err(BsonError::Malformed("sub-document too short".into()));
    }
    let body = reader.take((len - 4) as usize).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
    Ok(SliceReader::new(body))
}

fn parse_value(reader: &mut SliceReader<'_>, elem_type: u8) -> Result<BsonValue, BsonError> {
    match elem_type {
        0x01 => Ok(BsonValue::Double(reader.read_f64().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?)),
        0x02 => Ok(BsonValue::String(reader.read_string()?)),
        0x03 => Ok(BsonValue::Document(parse_sub_document(reader)?)),
        0x04 => Ok(BsonValue::Array(parse_sub_array(reader)?)),
        0x05 => {
            let len = reader.read_i32().ok_or_else(|| BsonError::Malformed("negative BSON binary length".into()))?;
            if len < 0 {
                return Err(BsonError::Malformed("negative BSON binary length".into()));
            }
            let subtype = reader.read_u8().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let payload = reader.take(len as usize).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            if subtype != 0x02 {
                return Ok(BsonValue::Binary(BinaryData::new(subtype, payload.to_vec())));
            }
            // 旧式子类型在载荷内再内嵌一份自身长度：解码时剥离，并拒绝内
            // 外长度不一致的流。
            if len < 4 {
                return Err(BsonError::Malformed("legacy BSON binary is shorter than its length field".into()));
            }
            let embedded = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            if embedded < 0 || embedded as usize + 4 != len as usize {
                return Err(BsonError::Malformed("legacy BSON binary length mismatch".into()));
            }
            Ok(BsonValue::Binary(BinaryData::new(subtype, payload[4..].to_vec())))
        }
        0x06 => Ok(BsonValue::Undefined),
        0x07 => {
            let bytes = reader.take(12).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let mut identifier = [0_u8; 12];
            identifier.copy_from_slice(bytes);
            Ok(BsonValue::ObjectId(identifier))
        }
        0x08 => Ok(BsonValue::Bool(reader.read_u8().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))? != 0)),
        0x09 => Ok(BsonValue::DateTime(reader.read_i64().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?)),
        0x0A => Ok(BsonValue::Null),
        0x0B => Ok(BsonValue::Regex { pattern: reader.read_cstring()?, options: reader.read_cstring()? }),
        0x0C => {
            let namespace = reader.read_string()?;
            let bytes = reader.take(12).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let mut id = [0_u8; 12];
            id.copy_from_slice(bytes);
            Ok(BsonValue::DbPointer { namespace, id })
        }
        0x0D => Ok(BsonValue::Code(reader.read_string()?)),
        0x0E => Ok(BsonValue::Symbol(reader.read_string()?)),
        0x0F => {
            let len = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            if len < 4 {
                return Err(BsonError::Malformed("BSON code-with-scope is shorter than its length field".into()));
            }
            let payload = reader.take((len - 4) as usize).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let mut inner = SliceReader::new(payload);
            let code = inner.read_string()?;
            let scope = parse_sub_document(&mut inner)?;
            if inner.remaining() != 0 {
                return Err(BsonError::Malformed("BSON code-with-scope length mismatch".into()));
            }
            Ok(BsonValue::CodeWithScope { code, scope })
        }
        0x10 => Ok(BsonValue::Int32(reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?)),
        0x11 => {
            let increment = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let seconds = reader.read_i32().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            Ok(BsonValue::Timestamp { i: increment as u32, t: seconds as u32 })
        }
        0x12 => Ok(BsonValue::Int64(reader.read_i64().ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?)),
        0x13 => {
            let bytes = reader.take(16).ok_or_else(|| BsonError::Malformed("unexpected end of BSON".into()))?;
            let mut identifier = [0_u8; 16];
            identifier.copy_from_slice(bytes);
            Ok(BsonValue::Decimal128(Decimal128::from_bytes(identifier)))
        }
        0x7F => Ok(BsonValue::MaxKey),
        0xFF => Ok(BsonValue::MinKey),
        other => Err(BsonError::Malformed(format!("unsupported BSON element type {other:#x}"))),
    }
}

fn parse_sub_document(reader: &mut SliceReader<'_>) -> Result<BsonDocument, BsonError> {
    let mut sub = bounded_sub_document(reader)?;
    let mut doc = BsonDocument::new();
    loop {
        let Some(elem_type) = sub.read_u8()
        else {
            break;
        };
        if elem_type == 0 {
            break;
        }
        let key = sub.read_cstring()?;
        let value = parse_value(&mut sub, elem_type)?;
        doc.push(key, value);
    }
    Ok(doc)
}

/// Arrays are the same wire shape as documents, but their keys are the element
/// indices and are never read back, so they are validated and dropped.
fn parse_sub_array(reader: &mut SliceReader<'_>) -> Result<Vec<BsonValue>, BsonError> {
    let mut sub = bounded_sub_document(reader)?;
    let mut items = Vec::new();
    loop {
        let Some(elem_type) = sub.read_u8()
        else {
            break;
        };
        if elem_type == 0 {
            break;
        }
        sub.skip_cstring()?;
        items.push(parse_value(&mut sub, elem_type)?);
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// 值 → 键字节：供上层 KV 存储路由使用
// ---------------------------------------------------------------------------

/// 把 BSON 值序列化为稳定键字节，供文档经 KV 存储路由。
/// 不同类型使用不同编码前缀，避免两个不同的 MongoDB 值撞上同一字节模式。
///
/// 数值共用同一键空间：MongoDB 按数值比较 `int32`、`int64`、`double` 与
/// `decimal128`，因此 `1`、`1.0`、`NumberLong(1)` 与 `NumberDecimal("1.0")`
/// 必须落到同一份文档，并作为 `_id` 判重时相撞。整数值统一编码为整数键，
/// 只有非整数用类型前缀键。`Symbol` 与同文本的 `String` 也共用键空间。
pub fn value_to_key(value: &BsonValue) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        BsonValue::ObjectId(bytes) => {
            out.push(b'o');
            out.extend_from_slice(bytes);
        }
        BsonValue::String(s) | BsonValue::Symbol(s) => {
            out.push(b's');
            out.extend_from_slice(s.as_bytes());
        }
        BsonValue::Int32(v) => out.extend_from_slice(&integral_key(*v as i64)),
        BsonValue::Int64(v) => out.extend_from_slice(&integral_key(*v)),
        BsonValue::Double(v) => match integral_value(*v) {
            Some(integer) => out.extend_from_slice(&integral_key(integer)),
            None => {
                out.push(b'd');
                out.extend_from_slice(&double_bits(*v).to_be_bytes());
            }
        },
        BsonValue::Decimal128(v) => match v.as_i64() {
            Some(integer) => out.extend_from_slice(&integral_key(integer)),
            None => {
                out.push(b'D');
                let (sign, exponent, coefficient) = v.key_parts();
                out.push(sign);
                out.extend_from_slice(&exponent.to_be_bytes());
                out.extend_from_slice(&coefficient.to_be_bytes());
            }
        },
        BsonValue::DateTime(v) => {
            out.push(b't');
            out.extend_from_slice(&v.to_be_bytes());
        }
        BsonValue::Timestamp { t, i } => {
            out.push(b'T');
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(&i.to_be_bytes());
        }
        BsonValue::MinKey => {
            out.push(b'm');
        }
        BsonValue::MaxKey => {
            out.push(b'M');
        }
        BsonValue::Bool(v) => {
            out.push(b'b');
            out.push(if *v { 1 } else { 0 });
        }
        BsonValue::Null => {
            out.push(b'n');
        }
        BsonValue::Undefined => {
            out.push(b'u');
        }
        other => {
            // Fallback: encode the whole value as BSON bytes for stable hashing.
            out.push(b'x');
            let mut doc = BsonDocument::new();
            doc.push("_id".to_string(), other.clone());
            encode_document(&mut out, &doc);
        }
    }
    out
}

/// 返回 double 精确表示的 64 位整数；无法精确表示时返回 `None`。
///
/// MongoDB 里 `1.0` 与 `NumberLong(1)` 是同一个 `_id`，而 `1e20` 超出 `i64`
/// 可表示范围，必须保留独立键。
pub fn integral_value(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    let integer = value as i64;
    (integer as f64 == value).then_some(integer)
}

/// 所有整数值共用的保序键前缀。
fn integral_key(value: i64) -> [u8; 9] {
    let mut key = [0_u8; 9];
    key[0] = b'n';
    key[1..].copy_from_slice(&((value as u64) ^ (1_u64 << 63)).to_be_bytes());
    key
}

/// 带小数 double 的规范位模式（`-0.0` 归并到 `0.0`）。
pub fn double_bits(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}
