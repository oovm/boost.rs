//! SQL 值类型、行 TLV 编码与解码。

use super::*;

/// SQL 查询的结果集。
#[derive(Debug, Clone, PartialEq)]
pub struct SqlResult {
    /// 列名（按投影顺序）。
    pub columns: Vec<String>,
    /// 行（每行的列值按 `columns` 顺序）。
    pub rows: Vec<Vec<SqlValue>>,
    /// DML 影响行数；SELECT 保持 0。
    pub affected_rows: u64,
}

/// SQL 层面的值域。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// 64 位有符号整数。整数列必须走这个变体，避免 `i64` 经 `f64` 往返时丢失精度。
    Integer(i64),
    /// 非整数数值（浮点字面量、除法、AVG 等）。
    Number(f64),
    /// 字符串。
    String(String),
    /// 原始 octets。只有声明为 `bytes` 的列（或 `CAST(... AS BYTEA|BLOB)`）才使用这个变体，
    /// 值不在任何位置经过 UTF-8 解码，因此可以承载 NUL、非 UTF-8 字节与空值。
    Bytes(Vec<u8>),
    /// 布尔。
    Boolean(bool),
    /// 数组，只有聚合 `array_agg(...)` 产出这个变体；元素按聚合输入顺序排列。
    Array(Vec<SqlValue>),
    /// NULL。
    Null,
}

/// 文本按 Unicode code point 精确比较：相等、排序、LIKE 与分桶共用同一口径。
///
/// gateway 不建模 collation，也不做大小写折叠。`CAST(x AS BINARY)` 因此只是恒等
/// 字节视图，不会带来第二套比较语义。
pub fn eq_string(a: &str, b: &str) -> bool {
    a == b
}

/// Orders two strings by Unicode code point, matching [`eq_string`].
pub fn cmp_string(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

/// 解析 SQL 数值字面量：不带小数点或指数的文本优先按 `i64` 精确解析，
/// 只有真正的小数/指数形式才转成 `f64`。
pub(crate) fn numeric_literal(text: &str) -> SqlValue {
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        if let Ok(value) = text.parse::<i64>() {
            return SqlValue::Integer(value);
        }
    }
    match text.parse::<f64>() {
        Ok(value) if value.is_finite() => SqlValue::Number(value),
        _ => SqlValue::String(text.to_owned()),
    }
}

/// 数值文本（`numeric` / `decimal` 列的 canonical 文本）转成可参与算术的值。
///
/// `Decimal` 列按驱动口径解码成 [`SqlValue::String`]，但 PostgreSQL 的 `numeric`
/// 在表达式里是数值：`balance + 1`、`balance > 100` 都必须按数值而不是按文本比较。
/// 这里只认完整可解析的十进制文本，其他字符串保持非数值语义。
pub(crate) fn numeric_text(text: &str) -> Option<SqlValue> {
    match numeric_literal(text.trim()) {
        value @ (SqlValue::Integer(_) | SqlValue::Number(_)) => Some(value),
        _ => None,
    }
}

impl SqlValue {
    /// 把值当作数值参与算术：整数/浮点原样返回，数值文本解析后返回。
    pub fn as_numeric(&self) -> Option<SqlValue> {
        match self {
            SqlValue::Integer(_) | SqlValue::Number(_) => Some(self.clone()),
            SqlValue::String(text) => numeric_text(text),
            _ => None,
        }
    }

    /// 从 KV 层拿到的原始 bytes 推断一个 SqlValue（无类型信息，宽松推断）。
    ///
    /// 如果 bytes 是合法 UTF-8 并且能解析为整数或浮点数，就返回 [`SqlValue::Integer`] /
    /// [`SqlValue::Number`]；否则返回 String。如果上游知道精确类型，应使用
    /// [`Self::from_row_bytes_typed`]。
    pub fn from_row_bytes(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes).into_owned();
        if let Ok(int) = text.parse::<i64>() {
            return SqlValue::Integer(int);
        }
        if let Ok(float) = text.parse::<f64>() {
            if float.is_finite() {
                return SqlValue::Number(float);
            }
        }
        SqlValue::String(text)
    }

    /// 按声明的 `field_type` 精确解码 bytes。
    ///
    /// - `String` 字段永远返回 [`SqlValue::String`]，保留前导零、空串、非数字文本。
    /// - `Bytes` 字段返回 [`SqlValue::Bytes`]，原始 octets 不做 UTF-8 解码，空 bytes 是空值而不是 NULL。
    /// - `Int64`/`Int32` 按整数解析。
    /// - `Float64` 按浮点解析。
    /// - `Bool` 识别 `true`/`false`/`1`/`0`。
    /// - `Date`/`Datetime` 原样保留写入时的日期/时间文本。
    /// - `Unknown` 退回到 [`Self::from_row_bytes`] 的宽松推断。
    ///
    /// 解码失败时降级为 [`SqlValue::String`] 保留原始 UTF-8 文本，
    /// 让上层继续处理而不是 panic。
    pub fn from_row_bytes_typed(bytes: &[u8], field_type: &crate::catalog_bind::SqlFieldType) -> Self {
        let inner = field_type.inner();
        match inner {
            // 字符串类型保留空串语义：空 bytes 是 `''`，不是 NULL。
            crate::catalog_bind::SqlFieldType::String => SqlValue::String(String::from_utf8_lossy(bytes).into_owned()),
            // Exact decimals arrive on the wire as text; keep them textual so
            // drivers observe the same value they would from MySQL.
            crate::catalog_bind::SqlFieldType::Decimal => SqlValue::String(String::from_utf8_lossy(bytes).into_owned()),
            // `json` / `jsonb` 的落库载荷是结构化 `any` 值（行记录类型标签），
            // 这里解码后只在 SQL 驱动边界渲染一次文本。
            crate::catalog_bind::SqlFieldType::Json => match super::json::text_from_bytes(bytes) {
                Ok(text) => SqlValue::String(text),
                // 早期版本把 JSON 存成文本；保持可读，避免旧数据变成硬错误。
                Err(_) => SqlValue::String(String::from_utf8_lossy(bytes).into_owned()),
            },
            // `DATE`/`DATETIME` columns store the canonical text spelling; the gateway
            // re-emits it verbatim and the driver-side column type turns it into a date.
            crate::catalog_bind::SqlFieldType::Date | crate::catalog_bind::SqlFieldType::Datetime => {
                SqlValue::String(String::from_utf8_lossy(bytes).into_owned())
            }
            // `uuid` is stored as its canonical 16 octets and reported as the
            // hyphenated lowercase spelling drivers expect. Any other payload
            // was not written by this version, so it is surfaced as raw text
            // instead of being reformatted into a fake uuid.
            crate::catalog_bind::SqlFieldType::Uuid => match crate::catalog_bind::uuid_text(bytes) {
                Some(text) => SqlValue::String(text),
                None => SqlValue::String(String::from_utf8_lossy(bytes).into_owned()),
            },
            crate::catalog_bind::SqlFieldType::Int64
            | crate::catalog_bind::SqlFieldType::Int32
            | crate::catalog_bind::SqlFieldType::Int16 => {
                if bytes.is_empty() {
                    return SqlValue::Null;
                }
                let text = String::from_utf8_lossy(bytes).into_owned();
                text.parse::<i64>().map(SqlValue::Integer).unwrap_or(SqlValue::String(text))
            }
            crate::catalog_bind::SqlFieldType::Float64 => {
                if bytes.is_empty() {
                    return SqlValue::Null;
                }
                let text = String::from_utf8_lossy(bytes).into_owned();
                text.parse::<f64>().ok().filter(|f| f.is_finite()).map(SqlValue::Number).unwrap_or(SqlValue::String(text))
            }
            crate::catalog_bind::SqlFieldType::Bool => {
                if bytes.is_empty() {
                    return SqlValue::Null;
                }
                let text = String::from_utf8_lossy(bytes).into_owned();
                match text.as_str() {
                    "true" | "True" | "TRUE" | "1" => SqlValue::Boolean(true),
                    "false" | "False" | "FALSE" | "0" => SqlValue::Boolean(false),
                    _ => SqlValue::String(text),
                }
            }
            // `bytes` 列是原始 octets：不做 UTF-8 解码，NUL、非 UTF-8 字节与空值都原样保留。
            crate::catalog_bind::SqlFieldType::Bytes => SqlValue::Bytes(bytes.to_vec()),
            // 没有类型信息时只能宽松推断。gateway 写入 NULL 时会省略字段，
            // 因此这里把空 bytes 视为空串，真正缺失的字段由调用方映射为 NULL。
            crate::catalog_bind::SqlFieldType::Unknown => Self::from_row_bytes(bytes),
            crate::catalog_bind::SqlFieldType::Optional(_) => unreachable!("inner() strips Optional"),
            crate::catalog_bind::SqlFieldType::Array(_) => unreachable!("array columns are not stored by this gateway"),
        }
    }

    /// 从 SQL 字面量构造。
    pub fn from_literal(lit: &SqlBoundLiteral) -> Self {
        match lit {
            SqlBoundLiteral::Integer(value) => SqlValue::Integer(*value),
            SqlBoundLiteral::Number(text) => numeric_literal(text),
            SqlBoundLiteral::String(text) => SqlValue::String(text.clone()),
            SqlBoundLiteral::Bytes(bytes) => SqlValue::Bytes(bytes.clone()),
            SqlBoundLiteral::Boolean(v) => SqlValue::Boolean(*v),
            SqlBoundLiteral::Null => SqlValue::Null,
        }
    }
}

/// 行内一个字段载荷的编码格式。
///
/// 行格式是 YY 自己的存储合同：新写入的字段用行记录类型标签编码
/// （[`database_formats::yy::row::encode_tagged`]），SQL 绑定只负责交付类型化
/// [`database_types::Value`]。旧版行里的字段是「按列类型解释」的原始字节，
/// 解码时标记为 [`CellFormat::LegacyRaw`]，重写该行时原样保留，未改动的字段
/// 不会因为整行重编码而改变字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellFormat {
    /// 行记录类型标签编码。
    Tagged,
    /// 旧版行载荷：按列类型解释的原始字节。
    LegacyRaw,
}

/// 一个 row 上各个字段的 bytes 值，key 是该列在本查询里的 row id。
///
/// 行的字段数通常只有个位数，用按 field_id 有序的 `Vec` 比 `BTreeMap` 少一层
/// 节点分配，逐行解码与克隆也更快；`encode_row` 依赖的字典序由 `insert`
/// 的二分定位维持。
#[derive(Debug, Default, Clone)]
pub struct RowFieldValues {
    fields: Vec<(u64, CellFormat, Vec<u8>)>,
}

impl RowFieldValues {
    /// 创建一个空 row。
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// 插入/覆盖一个字段值（field_id → 行记录标签编码的 bytes）。
    pub fn insert(&mut self, field_id: u64, value: Vec<u8>) {
        self.insert_cell(field_id, CellFormat::Tagged, value);
    }

    /// 以指定格式插入/覆盖一个字段值。
    ///
    /// legacy 行解码、跨行槽平移与合并都走这里，保留原格式；只有真正写入或
    /// `SET` 的字段才变成 [`CellFormat::Tagged`]。
    pub fn insert_cell(&mut self, field_id: u64, format: CellFormat, value: Vec<u8>) {
        match self.fields.binary_search_by_key(&field_id, |(existing, _, _)| *existing) {
            Ok(index) => self.fields[index] = (field_id, format, value),
            Err(index) => self.fields.insert(index, (field_id, format, value)),
        }
    }

    /// 删除一个字段。SQL NULL 在 row 编码里表示为字段缺失，因此
    /// `SET col = NULL` 必须移除已有字段而不是写入空 bytes。
    pub fn remove(&mut self, field_id: u64) {
        if let Ok(index) = self.fields.binary_search_by_key(&field_id, |(existing, _, _)| *existing) {
            self.fields.remove(index);
        }
    }

    /// 按 key 升序遍历全部字段（field_id、载荷格式、原始载荷）。
    pub fn iter(&self) -> impl Iterator<Item = (u64, CellFormat, &[u8])> {
        self.fields.iter().map(|(field_id, format, bytes)| (*field_id, *format, bytes.as_slice()))
    }

    /// 按 field_id 取原始载荷 bytes；不存在返回 None。
    pub fn get(&self, field_id: u64) -> Option<&[u8]> {
        let index = self.fields.binary_search_by_key(&field_id, |(existing, _, _)| *existing).ok()?;
        Some(self.fields[index].2.as_slice())
    }

    /// 按 field_id 取载荷格式与原始 bytes。
    pub fn get_cell(&self, field_id: u64) -> Option<(CellFormat, &[u8])> {
        let index = self.fields.binary_search_by_key(&field_id, |(existing, _, _)| *existing).ok()?;
        Some((self.fields[index].1, self.fields[index].2.as_slice()))
    }

    /// 合并另一行的字段。JOIN 后的一行同时携带两侧表的字段，
    /// 每张表的 row id 段互不重叠，所以按键覆盖即等价于拼行。
    pub fn merge(&mut self, other: &Self) {
        for (field_id, format, bytes) in other.iter() {
            self.insert_cell(field_id, format, bytes.to_vec());
        }
    }

    /// 按 `field_type` 解释一个字段值；字段缺失返回 `None`（SQL NULL）。
    ///
    /// 这是读取字段值的唯一入口：legacy 行按列类型解释原始字节，新行先做
    /// 行记录解码，再按列类型投影成 [`SqlValue`]。
    pub fn value_of(&self, field_id: u64, field_type: &crate::catalog_bind::SqlFieldType) -> Option<SqlValue> {
        let (format, bytes) = self.get_cell(field_id)?;
        Some(match format {
            CellFormat::LegacyRaw => SqlValue::from_row_bytes_typed(bytes, field_type),
            CellFormat::Tagged => match database_formats::yy::row::decode_tagged(bytes) {
                Ok(value) => sql_value_of_runtime(&value, field_type),
                // 载荷不是行记录编码（例如手工写入或旧代码遗留）时退回按列类型
                // 解释，避免把整列静默读成 NULL。
                Err(_) => SqlValue::from_row_bytes_typed(bytes, field_type),
            },
        })
    }

    /// 按 `field_type` 精确解码 bytes，保留前导零和空串等字符串语义。
    pub fn value_typed(
        &self,
        field_id: u64,
        field_types: &std::collections::BTreeMap<u64, crate::catalog_bind::SqlFieldType>,
    ) -> Option<SqlValue> {
        let field_type = field_types.get(&field_id).cloned().unwrap_or(crate::catalog_bind::SqlFieldType::Unknown);
        self.value_of(field_id, &field_type)
    }
}

/// 把行记录里的运行值按列类型投影成 SQL 值。
///
/// `json` / `jsonb` 是唯一需要表示层转换的类型：结构化 `any` 值在这里渲染一次
/// 紧凑文本，SQL 驱动拿到的就是它写入时的 JSON 形状。其余类型逐变体对应，
/// 不经过文本往返。
fn sql_value_of_runtime(value: &database_types::Value, field_type: &crate::catalog_bind::SqlFieldType) -> SqlValue {
    use database_types::Value;
    match field_type.inner() {
        crate::catalog_bind::SqlFieldType::Json => match super::json::text_of(value) {
            Ok(text) => SqlValue::String(text),
            Err(_) => natural_sql_value(value),
        },
        crate::catalog_bind::SqlFieldType::Uuid => match value {
            Value::Uuid(id) => SqlValue::String(id.hyphenated().to_string()),
            Value::Bytes(bytes) => match crate::catalog_bind::uuid_text(bytes) {
                Some(text) => SqlValue::String(text),
                None => SqlValue::Bytes(bytes.clone()),
            },
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::Int16
        | crate::catalog_bind::SqlFieldType::Int32
        | crate::catalog_bind::SqlFieldType::Int64 => match value {
            Value::I64(integer) => SqlValue::Integer(*integer),
            Value::U64(integer) => i64::try_from(*integer).map(SqlValue::Integer).unwrap_or(SqlValue::Number(*integer as f64)),
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::Float64 => match value {
            Value::F64(real) => SqlValue::Number(*real),
            Value::I64(integer) => SqlValue::Number(*integer as f64),
            Value::U64(integer) => SqlValue::Number(*integer as f64),
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::Bool => match value {
            Value::Bool(flag) => SqlValue::Boolean(*flag),
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::Bytes => match value {
            Value::Bytes(bytes) => SqlValue::Bytes(bytes.clone()),
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::String
        | crate::catalog_bind::SqlFieldType::Decimal
        | crate::catalog_bind::SqlFieldType::Date
        | crate::catalog_bind::SqlFieldType::Datetime => match value {
            Value::Text(text) => SqlValue::String(text.clone()),
            Value::I64(integer) => SqlValue::String(integer.to_string()),
            Value::U64(integer) => SqlValue::String(integer.to_string()),
            Value::F64(real) => SqlValue::String(super::format_number(*real)),
            other => natural_sql_value(other),
        },
        crate::catalog_bind::SqlFieldType::Unknown => natural_sql_value(value),
        crate::catalog_bind::SqlFieldType::Optional(_) => unreachable!("inner() strips Optional"),
        crate::catalog_bind::SqlFieldType::Array(_) => natural_sql_value(value),
    }
}

/// 无列类型信息时的自然投影：JSON 标量直接对应 SQL 值，嵌套结构渲染成 JSON 文本。
fn natural_sql_value(value: &database_types::Value) -> SqlValue {
    use database_types::Value;
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Boolean(*flag),
        Value::I64(integer) => SqlValue::Integer(*integer),
        Value::U64(integer) => i64::try_from(*integer).map(SqlValue::Integer).unwrap_or(SqlValue::Number(*integer as f64)),
        Value::F64(real) => SqlValue::Number(*real),
        Value::Text(text) => SqlValue::String(text.clone()),
        Value::Bytes(bytes) => SqlValue::Bytes(bytes.clone()),
        Value::Uuid(id) => SqlValue::String(id.hyphenated().to_string()),
        // 精确小数按声明精度与标度渲染成十进制文本：它就是数值，不是类型名。
        Value::Decimal(decimal) => SqlValue::String(decimal.to_plain_string()),
        // 列表与嵌套行只在 `json` 列里出现；这里与 JSON 列共用同一个渲染器。
        Value::List(_) | Value::Row(_) => match super::json::text_of(value) {
            Ok(text) => SqlValue::String(text),
            Err(_) => SqlValue::String(value.kind().to_string()),
        },
        other => SqlValue::String(other.kind().to_string()),
    }
}

/// 行载荷信封魔数。旧版首字节是列数高字节；真实表列数远小于 32768，
/// 首字节取不到 `0x80`，因此两者可以无歧义区分。
const ROW_ENVELOPE_MAGIC: u8 = 0x80;
/// 行载荷信封版本。
const ROW_ENVELOPE_VERSION: u8 = 0x01;
/// 单元格格式字节：行记录类型标签编码。
const CELL_TAGGED: u8 = 0;
/// 单元格格式字节：旧版原始字节。
const CELL_LEGACY_RAW: u8 = 1;

/// 行载荷是否使用当前信封。
fn is_envelope(data: &[u8]) -> bool {
    data.first() == Some(&ROW_ENVELOPE_MAGIC)
}

/// 行载荷 TLV 序列化格式。
///
/// 信封：`0x80 | 0x01 | u16 count(BE)`，随后每个字段
/// `u64 field_id(BE) | u8 cell_format | u32 len(BE) | payload`，字段按 field_id
/// 字典序排列。格式字节让 legacy 字段与标签编码字段共存于一行，重写整行时
/// 不会把未改动的旧载荷重新解释。
pub fn encode_row(values: &RowFieldValues) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ROW_ENVELOPE_MAGIC);
    out.push(ROW_ENVELOPE_VERSION);
    out.extend_from_slice(&(values.fields.len() as u16).to_be_bytes());
    for (field_id, format, bytes) in &values.fields {
        out.extend_from_slice(&field_id.to_be_bytes());
        out.push(match format {
            CellFormat::Tagged => CELL_TAGGED,
            CellFormat::LegacyRaw => CELL_LEGACY_RAW,
        });
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// 从 TLV 格式反序列化出 RowFieldValues。
pub fn decode_row(data: &[u8]) -> Result<RowFieldValues, SqlFrontendError> {
    if is_envelope(data) {
        decode_envelope(data)
    }
    else {
        // 旧版行：没有格式字节，所有字段都是按列类型解释的原始字节。
        decode_legacy(data, None)
    }
}

/// 解码当前信封；`needed_ids` 为空表示整行都要。
fn decode_envelope(data: &[u8]) -> Result<RowFieldValues, SqlFrontendError> {
    decode_envelope_fields(data, None)
}

fn decode_envelope_fields(data: &[u8], needed_ids: Option<&[u64]>) -> Result<RowFieldValues, SqlFrontendError> {
    let mut cursor = Cursor::new(data);
    let _magic = cursor.read_u8()?;
    let version = cursor.read_u8()?;
    if version != ROW_ENVELOPE_VERSION {
        return Err(internal_error(format!("unsupported row envelope version {version}")));
    }
    let count = cursor.read_u16()?;
    let needed_set: Option<std::collections::HashSet<u64>> =
        needed_ids.filter(|ids| ids.len() > 16).map(|ids| ids.iter().copied().collect());
    let mut row = RowFieldValues::new();
    for _ in 0..count {
        let field_id = cursor.read_u64()?;
        let format = match cursor.read_u8()? {
            CELL_TAGGED => CellFormat::Tagged,
            CELL_LEGACY_RAW => CellFormat::LegacyRaw,
            other => return Err(internal_error(format!("row cell format {other} is unknown"))),
        };
        let len = cursor.read_u32()? as usize;
        let needed = match (needed_ids, &needed_set) {
            (Some(_), Some(set)) => set.contains(&field_id),
            (Some(ids), None) => ids.contains(&field_id),
            (None, _) => true,
        };
        let bytes = cursor.read_bytes(len)?;
        if needed {
            row.insert_cell(field_id, format, bytes);
        }
    }
    Ok(row)
}

/// 解码旧版行；`needed_ids` 为空表示整行都要。
fn decode_legacy(data: &[u8], needed_ids: Option<&[u64]>) -> Result<RowFieldValues, SqlFrontendError> {
    let mut cursor = Cursor::new(data);
    let count = cursor.read_u16()?;
    let needed_set: Option<std::collections::HashSet<u64>> =
        needed_ids.filter(|ids| ids.len() > 16).map(|ids| ids.iter().copied().collect());
    let mut row = RowFieldValues::new();
    for _ in 0..count {
        let field_id = cursor.read_u64()?;
        let len = cursor.read_u32()? as usize;
        let needed = match (needed_ids, &needed_set) {
            (Some(_), Some(set)) => set.contains(&field_id),
            (Some(ids), None) => ids.contains(&field_id),
            (None, _) => true,
        };
        let bytes = cursor.read_bytes(len)?;
        if needed {
            row.insert_cell(field_id, CellFormat::LegacyRaw, bytes);
        }
    }
    Ok(row)
}

/// 部分解码：只收集 needed_ids 里的字段，跳过其他 TLV 条目。
/// 当 needed_ids 为空时表示调用方确实需要整行（例如尚未确定投影）。
pub fn decode_row_fields(data: &[u8], needed_ids: &[u64]) -> Result<RowFieldValues, SqlFrontendError> {
    if is_envelope(data) {
        return decode_envelope_fields(data, Some(needed_ids));
    }
    decode_legacy(data, Some(needed_ids))
}

/// 从 ScanPlan 里收集执行时需要解码的 field_ids（projection + predicate + order_by + expressions）。
pub fn scan_needed_field_ids(plan: &ScanPlan) -> Vec<u64> {
    let mut ids: Vec<u64> = Vec::new();
    for (fid, _) in &plan.projection_fields {
        if !ids.contains(fid) {
            ids.push(*fid);
        }
    }
    for (expr, _) in &plan.projection_expressions {
        collect_column_ids(expr, &mut ids);
    }
    if let Some(pred) = &plan.remaining_predicate {
        collect_column_ids(pred, &mut ids);
    }
    if let Some(group_exprs) = &plan.group_by {
        for g in group_exprs {
            collect_column_ids(g, &mut ids);
        }
    }
    if let Some(having) = &plan.having {
        collect_column_ids(having, &mut ids);
    }
    if let Some(order_items) = &plan.order_by {
        for item in order_items {
            collect_column_ids(&item.expression, &mut ids);
        }
    }
    ids
}

/// 从 ReadPlan / DeletePlan 的表达式里收集执行时需要解码的 field_ids。
pub fn expression_needed_field_ids(
    projection_fields: &[(u64, String)],
    projection_expressions: &[(SqlCatalogBoundExpression, String)],
    predicate: Option<&SqlCatalogBoundExpression>,
) -> Vec<u64> {
    let mut ids: Vec<u64> = Vec::new();
    for (fid, _) in projection_fields {
        if !ids.contains(fid) {
            ids.push(*fid);
        }
    }
    for (expr, _) in projection_expressions {
        collect_column_ids(expr, &mut ids);
    }
    if let Some(predicate) = predicate {
        collect_column_ids(predicate, &mut ids);
    }
    ids
}
