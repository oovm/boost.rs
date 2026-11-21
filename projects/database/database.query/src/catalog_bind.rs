//! 针对已发布 VOS 目录的 SQL 兼容绑定。

use crate::sql::{
    SqlBoundAssignment, SqlBoundBinaryOperator, SqlBoundConflictAction, SqlBoundCte, SqlBoundDelete, SqlBoundExpression, SqlBoundInsert,
    SqlBoundInsertSource, SqlBoundJoinKind, SqlBoundLiteral, SqlBoundOrderByItem, SqlBoundOrderDirection, SqlBoundProjection, SqlBoundSelect,
    SqlBoundSetOperand, SqlBoundSetOperation, SqlBoundSetOperator, SqlBoundSource, SqlBoundStatement, SqlBoundUnaryOperator, SqlBoundUpdate,
    SqlBoundWindowFrame, SqlBoundWindowSpec, SqlDialect, SqlErrorKind, SqlFrontendError, SqlNullsOrder, SqlScalarFunction, bind_sql,
};
use schema_types::{CanonicalScalar, ResolvedCanonicalType};

/// Types the SQL gateway can project from a VOS field contract.
///
/// This enum is **not** the VOS type system. It has two layers:
///
/// - variants backed one-to-one by a native VOS scalar (`Int64`, `Float64`,
///   `String`, `Bool`, `Bytes`, `Date`, `Datetime`, `Uuid`, …);
/// - gateway-only simulations requested by SQL DDL or produced by SQL
///   expressions ([`SqlFieldType::Json`], [`SqlFieldType::Array`],
///   [`SqlFieldType::Decimal`]). They shape the SQL wire face and literal
///   validation only; they do not add a storage type to VOS/YYDS/YYDB.
///
/// In particular `json/jsonb` has no storage scalar of its own: the payload is
/// the runtime structured `any` value (`Value::Row` / `Value::List` and JSON
/// scalars) encoded with the row-record type tags, and text is rendered only at
/// the SQL driver boundary. There is no "native json type" to distinguish —
/// `json` / `jsonb` are gateway spellings for the same structured payload.
///
/// Fields whose native type has no clean SQL projection fall back to
/// [`SqlFieldType::Unknown`] and leave interpretation to the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlFieldType {
    /// 64-bit signed integer (`i64` / `int64`).
    Int64,
    /// 32-bit signed integer (`i32` / `int32`).
    Int32,
    /// 16-bit signed integer (`i16` / `int16`, PostgreSQL `smallint`).
    ///
    /// PostgreSQL 上报 `int2`，驱动按 `i16` 物化；存储载荷与整数比较与
    /// [`SqlFieldType::Int32`] 同一条路径。
    Int16,
    /// 64-bit floating point (`f64` / `float64`).
    Float64,
    /// UTF-8 string (`utf8`).
    String,
    /// Boolean (`bool`).
    Bool,
    /// Raw opaque bytes (`bytes`).
    ///
    /// This is a precise binary type, not a loose "some opaque value" fallback:
    /// values decode to [`crate::SqlValue::Bytes`] without any UTF-8 conversion.
    /// The declared length limit is a dialect wire fact, not part of this type:
    /// see [`crate::SqlDialect::bytes_capacity`].
    Bytes,
    /// Exact decimal result type. MySQL reports `SUM`/`AVG` over exact operands
    /// as `DECIMAL`, which drivers surface as a string rather than a number.
    /// A declared `DECIMAL(p,s)` column also projects here and keeps its value
    /// as decimal text end to end.
    Decimal,
    /// Calendar date without a time part (`DATE`). Values stay text:
    /// the gateway stores and reports the `YYYY-MM-DD` spelling verbatim.
    Date,
    /// Date and time of day (`DATETIME` / `TIMESTAMP`). Values stay text: the
    /// gateway stores and reports the `YYYY-MM-DD HH:MM:SS` spelling verbatim.
    Datetime,
    /// UUID (`uuid`).
    ///
    /// Stored and compared as its canonical 16 octets, never as 36-character
    /// text: the row payload, the primary-key row id and the secondary-index
    /// payload all carry the 16 raw bytes, and their byte order equals the
    /// canonical text order. The wire face still reports the hyphenated
    /// lowercase spelling, which is what drivers expect for a text-encoded
    /// uuid column.
    Uuid,
    /// SQL 网关的 JSON / JSONB（`json` / `jsonb`）。
    ///
    /// 底层没有 json 标量：落库载荷是运行时结构化 `any` 值（[`database_types::Value::Row`] /
    /// [`database_types::Value::List`] 与 JSON 标量）的行记录编码，读写都不以 UTF-8
    /// 文本落库。PostgreSQL wire 报 `jsonb`，文本只在驱动边界渲染，属于表示层。
    Json,
    /// Optional wrapper — inner type + NULL. Box breaks the recursion
    Optional(Box<SqlFieldType>),
    /// Gateway-only array shape produced by `array_agg(...)` projections.
    ///
    /// Not a stored column type; the gateway never persists array values.
    Array(Box<SqlFieldType>),
    /// Anything the gateway does not recognise stays opaque.
    Unknown,
}

impl SqlFieldType {
    /// Extract the innermost non-optional type.
    pub fn inner(&self) -> SqlFieldType {
        match self {
            SqlFieldType::Optional(inner) => inner.inner(),
            other => other.clone(),
        }
    }

    /// Whether NULL is a legal value for this field.
    pub fn is_nullable(&self) -> bool {
        matches!(self, SqlFieldType::Optional(_))
    }

    /// Canonical storage bytes for one SQL literal written into this column type.
    ///
    /// `NULL` yields `Ok(None)` — the row encoding omits the field entirely.
    /// Every other value keeps its plain bytes except [`SqlFieldType::Uuid`],
    /// where the 36-character text form is parsed into its 16 canonical octets
    /// so storage, comparison and index payloads share one representation.
    pub fn storage_value_of(&self, literal: &SqlBoundLiteral) -> Result<Option<database_types::Value>, SqlFrontendError> {
        use database_types::Value;
        let inner = self.inner();
        if matches!(literal, SqlBoundLiteral::Null) {
            return Ok(None);
        }
        if matches!(inner, SqlFieldType::Uuid) {
            return match literal {
                SqlBoundLiteral::Bytes(bytes) if bytes.len() == 16 => {
                    Ok(Some(Value::Uuid(uuid::Uuid::from_slice(bytes).expect("length checked above"))))
                }
                SqlBoundLiteral::String(text) => match uuid::Uuid::parse_str(text) {
                    Ok(id) => Ok(Some(Value::Uuid(id))),
                    Err(_) => Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not a valid UUID"))),
                },
                _ => Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, "a uuid column accepts only 16-octet binary or UUID text")),
            };
        }
        if matches!(inner, SqlFieldType::Json) {
            return Ok(Some(match literal {
                // 结构化一次：客户端文本在这里解析成 `any` 值，之后读写都走二进制值。
                SqlBoundLiteral::String(text) => crate::sql_execute::json::parse(text)?,
                // PostgreSQL `jsonb` 二进制参数（版本字节 + JSON 文本）。
                SqlBoundLiteral::Bytes(bytes) => crate::sql_execute::json::parse_bytes(bytes)?,
                _ => {
                    return Err(SqlFrontendError::new(
                        SqlErrorKind::TypeMismatch,
                        "a json column accepts only JSON text or a jsonb binary parameter",
                    ));
                }
            }));
        }
        Ok(Some(match (inner.clone(), literal) {
            (SqlFieldType::Int16, SqlBoundLiteral::Integer(value)) => {
                i16::try_from(*value).map(|value| Value::I64(i64::from(value))).map_err(|_| integer_out_of_range(self))?
            }
            (SqlFieldType::Int32, SqlBoundLiteral::Integer(value)) => {
                i32::try_from(*value).map(|value| Value::I64(i64::from(value))).map_err(|_| integer_out_of_range(self))?
            }
            (SqlFieldType::Int64, SqlBoundLiteral::Integer(value)) => Value::I64(*value),
            (SqlFieldType::Int16 | SqlFieldType::Int32 | SqlFieldType::Int64, SqlBoundLiteral::String(text)) => {
                let parsed: i64 =
                    text.trim().parse().map_err(|_| SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not an integer")))?;
                match inner {
                    SqlFieldType::Int16 => {
                        i16::try_from(parsed).map_err(|_| integer_out_of_range(self))?;
                    }
                    SqlFieldType::Int32 => {
                        i32::try_from(parsed).map_err(|_| integer_out_of_range(self))?;
                    }
                    _ => {}
                }
                Value::I64(parsed)
            }
            (SqlFieldType::Float64, SqlBoundLiteral::Integer(value)) => Value::F64(*value as f64),
            (SqlFieldType::Float64, SqlBoundLiteral::Number(text) | SqlBoundLiteral::String(text)) => match text.trim().parse::<f64>() {
                Ok(real) if real.is_finite() => Value::F64(real),
                _ => return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not a finite number"))),
            },
            (SqlFieldType::Bool, SqlBoundLiteral::Boolean(value)) => Value::Bool(*value),
            (SqlFieldType::Bool, SqlBoundLiteral::Integer(value)) if *value == 0 || *value == 1 => Value::Bool(*value == 1),
            (SqlFieldType::Bool, SqlBoundLiteral::String(text)) => match text.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Value::Bool(true),
                "false" | "f" | "0" => Value::Bool(false),
                _ => return Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not a boolean"))),
            },
            (SqlFieldType::Bytes, SqlBoundLiteral::Bytes(bytes)) => Value::Bytes(bytes.clone()),
            (SqlFieldType::Bytes, SqlBoundLiteral::String(text)) => Value::Bytes(text.as_bytes().to_vec()),
            // 文本承载的类型（字符串、精确小数、日期、时间戳）统一落成文本值。
            (_, SqlBoundLiteral::Integer(value)) => Value::Text(value.to_string()),
            (_, SqlBoundLiteral::Number(text)) => Value::Text(text.clone()),
            (_, SqlBoundLiteral::String(text)) => Value::Text(text.clone()),
            (_, SqlBoundLiteral::Bytes(bytes)) => Value::Bytes(bytes.clone()),
            (_, SqlBoundLiteral::Boolean(value)) => Value::Text(if *value { "true" } else { "false" }.to_string()),
            (SqlFieldType::Array(_), _) => unreachable!("array columns are not stored by this gateway"),
            (_, SqlBoundLiteral::Null) => unreachable!("handled above"),
        }))
    }

    /// Row-id bytes for one primary-key literal.
    ///
    /// This is the KV key layout, not the row payload: existing rows were written
    /// with the same spelling, so changing it would orphan them. uuid keeps its
    /// 16 canonical octets; every other type keeps the literal's plain bytes.
    pub fn primary_key_bytes_of(&self, literal: &SqlBoundLiteral) -> Result<Vec<u8>, SqlFrontendError> {
        if matches!(literal, SqlBoundLiteral::Null) {
            return Err(SqlFrontendError::new(SqlErrorKind::NotNullViolation, "primary key must not be NULL"));
        }
        if matches!(self.inner(), SqlFieldType::Uuid) {
            return match literal {
                SqlBoundLiteral::Bytes(bytes) if bytes.len() == 16 => Ok(bytes.clone()),
                SqlBoundLiteral::String(text) => match parse_uuid(text) {
                    Some(bytes) => Ok(bytes.to_vec()),
                    None => Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not a valid UUID"))),
                },
                _ => Err(SqlFrontendError::new(SqlErrorKind::TypeMismatch, "a uuid column accepts only 16-octet binary or UUID text")),
            };
        }
        Ok(match literal {
            SqlBoundLiteral::Integer(value) => value.to_string().into_bytes(),
            SqlBoundLiteral::Number(text) => text.as_bytes().to_vec(),
            SqlBoundLiteral::String(text) => text.as_bytes().to_vec(),
            SqlBoundLiteral::Bytes(bytes) => bytes.clone(),
            SqlBoundLiteral::Boolean(value) => (if *value { "1" } else { "0" }).as_bytes().to_vec(),
            SqlBoundLiteral::Null => unreachable!("handled above"),
        })
    }
}

/// Integer literal outside the declared column width.
fn integer_out_of_range(field_type: &SqlFieldType) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::NumericOverflow, format!("integer value is out of range for {:?}", field_type.inner()))
}

/// Parses UUID text into its canonical big-endian 16 octets.
///
/// Accepts the hyphenated, simple, braced and `urn:uuid:` spellings the
/// `uuid` crate understands, and rejects everything else.
pub fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    uuid::Uuid::parse_str(text).ok().map(|uuid| *uuid.as_bytes())
}

/// Renders canonical UUID octets as the hyphenated lowercase spelling.
pub fn uuid_text(bytes: &[u8]) -> Option<String> {
    uuid::Uuid::from_slice(bytes).ok().map(|uuid| uuid.hyphenated().to_string())
}

/// Map a resolved VOS type to a gateway-facing [`SqlFieldType`].
///
/// Every VOS type is classified on purpose. Types with no SQL equivalent —
/// the `file` user type, the `vector` generic type, `u64`, `decimal`, and the
/// remaining composites — project to [`SqlFieldType::Unknown`] here and are
/// rejected with a field-naming diagnostic by [`reject_unknown_field`] on
/// every bind path; nothing silently degrades into a runtime guess.
/// 将 VOS 解析后的规范类型投影为 SQL 字段类型（无等价时为 Unknown）。
pub fn project_resolved_type(ty: &ResolvedCanonicalType) -> SqlFieldType {
    match ty {
        ResolvedCanonicalType::Scalar(scalar) => match scalar {
            CanonicalScalar::I16 => SqlFieldType::Int16,
            CanonicalScalar::I8 | CanonicalScalar::I32 => SqlFieldType::Int32,
            CanonicalScalar::I64 | CanonicalScalar::U8 | CanonicalScalar::U16 | CanonicalScalar::U32 => SqlFieldType::Int64,
            CanonicalScalar::F32 | CanonicalScalar::F64 => SqlFieldType::Float64,
            CanonicalScalar::Utf8 => SqlFieldType::String,
            // `utf16` has no execution scalar in the VOS runtime either: its
            // write path rejects it instead of silently storing UTF-8 bytes
            // under a UTF-16 name. The SQL projection does the same, with the
            // VOS type name attached to the bind-time diagnostic.
            CanonicalScalar::Utf16 => SqlFieldType::Unknown,
            CanonicalScalar::Bool => SqlFieldType::Bool,
            CanonicalScalar::Bytes => SqlFieldType::Bytes,
            CanonicalScalar::Date => SqlFieldType::Date,
            CanonicalScalar::DateTime => SqlFieldType::Datetime,
            CanonicalScalar::Uuid => SqlFieldType::Uuid,
            CanonicalScalar::Time => SqlFieldType::String,
            // `u64` cannot narrow into `i64` without lying about the range and
            // `decimal` has no lossless gateway numeric type. Both stay
            // `Unknown` and are rejected at bind time with the VOS type name.
            CanonicalScalar::U64 | CanonicalScalar::Decimal => SqlFieldType::Unknown,
        },
        ResolvedCanonicalType::Optional(inner) => SqlFieldType::Optional(Box::new(project_resolved_type(inner))),
        // VOS `file` is a user-declared identity with canonical path `file`.
        // Explicit branch, no wildcard: it has no SQL equivalent and is
        // rejected at bind time with the original type name.
        ResolvedCanonicalType::User { path, .. } if path.last().map(String::as_str) == Some("file") => SqlFieldType::Unknown,
        // VOS `vector<..>` is a generic type with canonical path `vector`.
        // Same explicit treatment as `file`.
        ResolvedCanonicalType::Generic { path, .. } if path.last().map(String::as_str) == Some("vector") => SqlFieldType::Unknown,
        // Remaining composites (`Reference`, `List`, other `User` / `Generic`)
        // also have no SQL equivalent: `Unknown` plus the bind-time rejection.
        _ => SqlFieldType::Unknown,
    }
}

/// Human-readable VOS type name extracted from a [`ResolvedCanonicalType`].
///
/// Used only for diagnostic messages — the caller maps the resolved type to
/// [`SqlFieldType`] via [`project_resolved_type`] and keeps the display name as
/// an opaque hint when the field falls back to `Unknown`.
fn canonical_type_display_name(ty: &ResolvedCanonicalType) -> String {
    match ty {
        ResolvedCanonicalType::Scalar(scalar) => format!("scalar({scalar:?})"),
        ResolvedCanonicalType::User { path, .. } => path.last().cloned().unwrap_or_else(|| "user".to_string()),
        ResolvedCanonicalType::Generic { path, arguments } => {
            let base = path.last().cloned().unwrap_or_else(|| "generic".to_string());
            if arguments.is_empty() { base } else { format!("{base}<{}>", arguments.len()) }
        }
        ResolvedCanonicalType::List(inner) => format!("list<{}>", canonical_type_display_name(inner)),
        ResolvedCanonicalType::Reference(inner) => canonical_type_display_name(inner),
        ResolvedCanonicalType::Optional(inner) => canonical_type_display_name(inner),
        ResolvedCanonicalType::File => "file".to_string(),
        ResolvedCanonicalType::Vector { dimension, .. } => format!("vector<{dimension}>"),
    }
}

/// A table identity exposed by the cluster catalog to a gateway binder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogTable {
    /// Stable 集群-owned type identity.
    pub type_id: u64,
    /// VOS table name used for SQL lookup.
    pub name: String,
    /// Columns in catalog order.
    pub fields: Vec<SqlCatalogField>,
    /// Secondary `UNIQUE` constraints as field identities in declaration order.
    ///
    /// Each inner vector is one constraint. Writes are rejected when they would
    /// produce a duplicate tuple, matching PostgreSQL `23505` and MySQL `1062`.
    pub unique_constraints: Vec<Vec<u64>>,
    /// Non-unique secondary indexes as field identities, one column each.
    ///
    /// The write path maintains entries for them, and the planner may probe them for
    /// equality / integer-range predicates. Composite indexes are rejected by the
    /// contract projection, so every inner vector holds exactly one field.
    pub indexes: Vec<Vec<u64>>,
    /// Table-level `CHECK` expression texts from local DDL, re-parsed by the
    /// SQL frontend when a write validates them. Absent for catalog-seeded
    /// tables published from the VOS contract.
    pub check_constraints: Vec<String>,
    /// `FOREIGN KEY` constraints projected onto field identities.
    pub foreign_keys: Vec<SqlCatalogForeignKey>,
}

/// One `FOREIGN KEY` constraint projected onto catalog field identities.
///
/// The write path enforces referential integrity with a point lookup of the
/// parent columns: an INSERT or UPDATE that supplies a non-`NULL` child value
/// with no matching parent row fails with PostgreSQL `23503`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogForeignKey {
    /// Child column field identities, in declaration order.
    pub columns: Vec<u64>,
    /// Referenced table identity.
    pub parent_type_id: u64,
    /// Referenced column field identities, aligned with [`Self::columns`].
    pub parent_columns: Vec<u64>,
    /// `ON DELETE` action text, as written.
    pub on_delete: Option<String>,
    /// `ON UPDATE` action text, as written.
    pub on_update: Option<String>,
}

/// A column identity exposed by the cluster catalog to a gateway binder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogField {
    /// Stable 集群-owned field identity.
    pub field_id: u64,
    /// VOS field name used for SQL lookup.
    pub name: String,
    /// Declared field type projected from the VOS contract.
    pub field_type: SqlFieldType,
    /// Human-readable VOS type name carried for diagnostics, populated only when
    /// [`Self::field_type`] falls back to [`SqlFieldType::Unknown`].
    pub vos_type_name: Option<String>,
    /// Declared character or octet capacity from local DDL, absent for fields
    /// projected from the published VOS contract.
    pub capacity: Option<u32>,
    /// Raw `DEFAULT` literal text from local DDL, when declared.
    pub default: Option<String>,
    /// Inline `CHECK` expression text from local DDL, when declared.
    pub check: Option<String>,
    /// Stored generated-column expression text from local DDL, when declared.
    /// A non-`None` value means the planner computes the column's value and
    /// rejects a direct write of it.
    pub generated: Option<String>,
}

/// The gateway's immutable view of one published VOS catalog revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalog {
    schema_version: u32,
    tables: Vec<SqlCatalogTable>,
}

impl SqlCatalog {
    /// Builds a gateway view from the newest contract published by 集群.
    pub fn from_catalog(catalog: &database_cluster::catalog::Catalog) -> Result<Self, SqlFrontendError> {
        let schema = catalog.schema().ok_or_else(|| internal_error("集群 catalog has no published schema"))?;
        let contract = catalog.resolved_contract().ok_or_else(|| internal_error("集群 catalog has no resolved contract"))?;
        Self::from_resolved_contract(schema.version, contract)
    }

    /// Projects gateway table identities from a validated resolved contract.
    pub fn from_resolved_contract(schema_version: u32, contract: &schema_types::ResolvedContract) -> Result<Self, SqlFrontendError> {
        if schema_version == 0 {
            return Err(internal_error("catalog schema version must be nonzero"));
        }
        let artifact = contract.to_json().map_err(|_| internal_error("resolved contract encode failed"))?;
        schema_types::ResolvedContract::from_json(&artifact)
            .map_err(|error| internal_error(&format!("invalid resolved VOS contract {}: {}", error.code, error.message)))?;
        let mut type_ids = std::collections::BTreeSet::new();
        let mut field_ids = std::collections::BTreeSet::new();
        let tables = contract
            .types
            .iter()
            .filter(|entry| entry.kind == schema_types::TypeContractKind::Table)
            .map(|entry| {
                if entry.type_id == 0 || !type_ids.insert(entry.type_id) {
                    return Err(internal_error("invalid or duplicate catalog type identity"));
                }
                let name = entry.canonical_path.last().cloned().ok_or_else(|| internal_error("catalog table has no canonical name"))?;
                let fields = entry
                    .fields
                    .iter()
                    .map(|field| {
                        if field.field_id == 0 || !field_ids.insert(field.field_id) {
                            return Err(internal_error("invalid or duplicate catalog field identity"));
                        }
                        let field_type = project_resolved_type(&field.canonical_type);
                        let vos_type_name = match field_type.inner() {
                            SqlFieldType::Unknown => Some(canonical_type_display_name(&field.canonical_type)),
                            _ => None,
                        };
                        Ok(SqlCatalogField {
                            field_id: field.field_id,
                            name: field.canonical_name.clone(),
                            field_type,
                            vos_type_name,
                            capacity: None,
                            default: None,
                            check: None,
                            generated: None,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SqlCatalogTable {
                    type_id: entry.type_id,
                    name,
                    fields,
                    unique_constraints: Vec::new(),
                    indexes: entry
                        .fields
                        .iter()
                        .filter(|field| field.attributes.iter().any(|attribute| attribute.name == "index"))
                        .map(|field| vec![field.field_id])
                        .collect(),
                    check_constraints: Vec::new(),
                    foreign_keys: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { schema_version, tables })
    }

    /// Returns the catalog schema revision used by this binding view.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Builds a gateway view from locally registered DDL tables.
    ///
    /// The registry validates identities while applying DDL, so this constructor
    /// repeats the checks that keep binders from observing ambiguous tables.
    pub fn from_tables(schema_version: u32, tables: Vec<SqlCatalogTable>) -> Result<Self, SqlFrontendError> {
        if schema_version == 0 {
            return Err(internal_error("catalog schema version must be nonzero"));
        }
        let mut type_ids = std::collections::BTreeSet::new();
        let mut field_ids = std::collections::BTreeSet::new();
        for table in &tables {
            if table.name.is_empty() {
                return Err(internal_error("catalog table has no name"));
            }
            if table.type_id == 0 || !type_ids.insert(table.type_id) {
                return Err(internal_error("invalid or duplicate catalog type identity"));
            }
            for field in &table.fields {
                if field.name.is_empty() {
                    return Err(internal_error("catalog field has no name"));
                }
                if field.field_id == 0 || !field_ids.insert(field.field_id) {
                    return Err(internal_error("invalid or duplicate catalog field identity"));
                }
            }
            for constraint in &table.unique_constraints {
                if constraint.is_empty() {
                    return Err(internal_error("catalog unique constraint has no columns"));
                }
                for field_id in constraint {
                    if !table.fields.iter().any(|field| field.field_id == *field_id) {
                        return Err(internal_error("catalog unique constraint references an unknown field"));
                    }
                }
            }
        }
        Ok(Self::assemble(schema_version, tables))
    }

    /// Wraps already validated table identities without repeating the checks.
    pub(crate) fn assemble(schema_version: u32, tables: Vec<SqlCatalogTable>) -> Self {
        Self { schema_version, tables }
    }

    /// Returns the catalog tables in VOS declaration order.
    pub fn tables(&self) -> &[SqlCatalogTable] {
        &self.tables
    }
}

/// Stride between two occurrences of the same catalog table inside one query.
///
/// A self join repeats one catalog table, whose `field_id`s are unique across the
/// catalog. Row values are keyed by a synthesized *row id* so both occurrences can
/// carry their own copy of the same column without overwriting each other. The
/// first occurrence keeps slot `0`, which makes every row id equal to its catalog
/// `field_id` and leaves single-table behavior untouched.
pub const ROW_ID_STRIDE: u64 = 1 << 40;

/// A table reference after SQL names are bound to 集群 catalog identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundTable {
    /// Stable 集群 table identity.
    pub type_id: u64,
    /// Catalog table name.
    pub name: String,
    /// Alias that replaces [`Self::name`] in the column scope, when written.
    pub alias: Option<String>,
    /// Zero-based occurrence of this catalog table inside the query.
    pub row_slot: u64,
    /// Visible column names aligned with the catalog field order when an explicit CTE
    /// column list renamed this occurrence. `None` keeps every catalog field name.
    pub column_names: Option<Vec<String>>,
    /// Catalog field indices this occurrence exposes, in the CTE body's projection
    /// order. `None` exposes every catalog field, which is what a base table and a
    /// `SELECT *` CTE do; `Some` narrows a flattened CTE to its projected subset.
    pub visible_columns: Option<Vec<usize>>,
}

impl SqlCatalogBoundTable {
    /// The name columns are addressed by: the alias when present, the table name otherwise.
    pub fn visible_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    /// The row-value key for one catalog field of this occurrence.
    pub fn row_id(&self, field_id: u64) -> u64 {
        field_id + self.row_slot * ROW_ID_STRIDE
    }

    /// The name the column at `field_index` is addressed by in this occurrence.
    ///
    /// A CTE with an explicit column list renames its output positionally, so the
    /// rename lives beside the catalog field order and leaves the catalog name,
    /// `field_id` and row slot untouched.
    pub fn visible_column_name(&self, field_index: usize, field: &SqlCatalogField) -> String {
        self.column_names.as_ref().and_then(|names| names.get(field_index).cloned()).unwrap_or_else(|| field.name.clone())
    }

    /// Whether this occurrence exposes the catalog field at `field_index` at all.
    pub fn exposes_column(&self, field_index: usize) -> bool {
        self.visible_columns.as_ref().is_none_or(|columns| columns.contains(&field_index))
    }
}

/// A column reference after SQL names are bound to 集群 catalog identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundColumn {
    /// Stable 集群 table identity.
    pub type_id: u64,
    /// Stable 集群 field identity.
    pub field_id: u64,
    /// Row-value key: [`Self::field_id`] shifted by the table occurrence slot.
    pub row_id: u64,
    /// Catalog table name.
    pub table_name: String,
    /// Catalog field name.
    pub field_name: String,
    /// Name the reference was written by in its column scope, which a CTE's explicit
    /// column list can rename away from [`Self::field_name`].
    pub visible_name: String,
    /// Declared character/octet capacity of the bound field, used for write guards.
    pub capacity: Option<u32>,
}

/// A column of the directly enclosing query read by a correlated subquery.
///
/// A correlated subquery is re-executed once per outer row, so the value cannot
/// come from the subquery's own row. The declared type travels with the column
/// because execution decodes the outer row without consulting the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogOuterColumn {
    /// Column identity and the row-value key inside the enclosing query's row.
    pub column: SqlCatalogBoundColumn,
    /// Declared type of the column, taken from the catalog at bind time.
    pub field_type: SqlFieldType,
}

/// SQL expression after catalog binding and before distributed planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlCatalogBoundExpression {
    /// A catalog-resolved column reference.
    Column(SqlCatalogBoundColumn),
    /// A column of the directly enclosing query, supplied per outer row.
    OuterColumn(SqlCatalogOuterColumn),
    /// A SQL literal retaining gateway semantics.
    Literal(SqlBoundLiteral),
    /// A bare `*` argument; only `COUNT(*)` gives it meaning.
    Wildcard,
    /// A SQL function call (aggregate, scalar or window function).
    Function {
        /// Canonical lower-case function name.
        name: String,
        /// Catalog-bound arguments.
        args: Vec<SqlCatalogBoundExpression>,
        /// Whether the aggregate uses `DISTINCT` quantifier.
        distinct: bool,
        /// `OVER (...)` clause when the call is a window function.
        window: Option<SqlCatalogBoundWindowSpec>,
        /// `FILTER (WHERE ...)` predicate restricting the rows an aggregate sees.
        filter: Option<Box<Self>>,
    },
    /// A binary SQL expression.
    Binary {
        /// Left operand.
        left: Box<Self>,
        /// Operator.
        operator: SqlBoundBinaryOperator,
        /// Right operand.
        right: Box<Self>,
    },
    /// A unary SQL expression.
    Unary {
        /// Operator.
        operator: SqlBoundUnaryOperator,
        /// Operand.
        expression: Box<Self>,
    },
    /// `expr IN (<subquery>)` with the subquery already planned.
    InSubquery {
        /// Left-hand side expression.
        expression: Box<Self>,
        /// Executable plan for the subquery.
        plan: Box<crate::sql_planner::SqlExecutionPlan>,
        /// Whether the predicate is spelled `NOT IN`.
        negated: bool,
    },
    /// A scalar subquery `(SELECT ...)` with the subquery already planned.
    ///
    /// The plan is executed once per enclosing row when it reads an enclosing column,
    /// and once per statement otherwise.
    ScalarSubquery {
        /// Executable plan for the subquery.
        plan: Box<crate::sql_planner::SqlExecutionPlan>,
    },
    /// `[NOT] EXISTS (<subquery>)` with the subquery already planned.
    Exists {
        /// Executable plan for the subquery.
        plan: Box<crate::sql_planner::SqlExecutionPlan>,
        /// Whether the predicate is spelled `NOT EXISTS`.
        negated: bool,
    },
    /// `IN` predicate whose subquery rows were materialized before evaluation.
    InValues {
        /// Left-hand side expression.
        expression: Box<Self>,
        /// Literal values produced by the subquery.
        values: Vec<SqlBoundLiteral>,
        /// Whether the predicate is spelled `NOT IN`.
        negated: bool,
        /// `values` 是否已归一化为「NULL 在前、整数升序」。归一化后逐行求值可走
        /// 二分查找，不必每行重建集合；非整数列表保持原顺序与通用比较路径。
        sorted_integers: bool,
    },
    /// `CASE WHEN cond THEN result ... [ELSE fallback] END`.
    Case {
        /// `(when, then)` branches in source order.
        branches: Vec<(Box<Self>, Box<Self>)>,
        /// Optional ELSE fallback.
        else_branch: Option<Box<Self>>,
    },
    /// `CAST(expr AS type_name)`.
    Cast {
        /// The expression being cast.
        expr: Box<Self>,
        /// The target type spelling as written (e.g. `INTEGER`, `TEXT`, `REAL`, `BLOB`).
        target: String,
    },
    /// `INTERVAL '<数量> <单位>'` — 文本原样透传，与日期时间加减时由执行层展开。
    Interval {
        /// Interval body text as written, e.g. `1 day`.
        text: String,
    },
}

/// SQL projection after catalog binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlCatalogBoundProjection {
    /// All columns of the selected catalog table.
    Star(SqlCatalogBoundTable),
    /// A resolved expression with an optional client alias.
    Expression {
        /// Resolved expression.
        expression: SqlCatalogBoundExpression,
        /// Optional SQL alias.
        alias: Option<String>,
    },
}

/// SQL SELECT request after catalog binding and before 集群 distributed planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundSelect {
    /// Selected table identity, if the query has a source.
    pub source: Option<SqlCatalogBoundTable>,
    /// JOIN clauses in source order, each bound to its own catalog table.
    pub joins: Vec<SqlCatalogBoundJoin>,
    /// Resolved projections.
    pub projections: Vec<SqlCatalogBoundProjection>,
    /// Resolved optional predicate.
    pub predicate: Option<SqlCatalogBoundExpression>,
    /// Resolved optional GROUP BY expressions.
    pub group_by: Option<Vec<SqlCatalogBoundExpression>>,
    /// Resolved optional HAVING expression.
    pub having: Option<SqlCatalogBoundExpression>,
    /// Optional `LIMIT` count expression, propagated from the gateway binder.
    pub limit: Option<SqlCatalogBoundExpression>,
    /// Optional `OFFSET` count expression, propagated from the gateway binder.
    pub offset: Option<SqlCatalogBoundExpression>,
    /// Optional ORDER BY items, propagated from the gateway binder.
    pub order_by: Option<Vec<SqlCatalogBoundOrderByItem>>,
    /// Whether the projection list carries the `DISTINCT` modifier.
    pub distinct: bool,
    /// `WITH RECURSIVE` definitions declared by this select, in source order.
    ///
    /// Each entry carries the fully bound seed and step plus the synthetic
    /// relation the step scans. The planner materializes them as a fixed point
    /// below the select that references them.
    pub recursive_ctes: Vec<SqlCatalogRecursiveCte>,
}

/// Monotonic counter handing out identities for recursive-CTE synthetic tables.
static SYNTHETIC_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Synthetic type ids live at the very top of the id space; cluster-issued ids
/// never reach it, so `cte:{type_id}` namespaces cannot collide with `table:`.
const SYNTHETIC_TYPE_ID_TOP: u64 = 1 << 63;

/// Synthetic field ids start here. A real row id is `field_id + slot * 2^40`,
/// which cannot reach this band for any realistic catalog and occurrence count.
const SYNTHETIC_FIELD_ID_TOP: u64 = 1 << 62;

/// Field-id stride between two synthetic tables; also the column cap of one.
const SYNTHETIC_FIELD_STRIDE: u64 = 1 << 8;

/// Whether `type_id` names a recursive-CTE synthetic relation rather than a
/// catalog table.
pub fn is_synthetic_type_id(type_id: u64) -> bool {
    type_id >= SYNTHETIC_TYPE_ID_TOP
}

/// The KV namespace a recursive-CTE materialization lives under.
pub fn synthetic_namespace(type_id: u64) -> String {
    format!("cte:{type_id}")
}

/// A `WITH RECURSIVE` definition after catalog binding.
///
/// The seed and step are complete bound selects. The step scans [`Self::fields`]
/// as a synthetic relation keyed by [`Self::type_id`]; every synthetic occurrence
/// is pinned to row slot 0, so its row ids equal the synthetic field ids and the
/// executor materializes working-set rows under exactly those keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogRecursiveCte {
    /// Name the CTE is referenced by in `FROM` / `JOIN`.
    pub name: String,
    /// Synthetic relation identity, top of the type-id space.
    pub type_id: u64,
    /// Output columns: name and inferred type taken from the seed projection.
    pub fields: Vec<SqlCatalogField>,
    /// Bound non-recursive operand — the initial working set.
    pub seed: SqlCatalogBoundSelect,
    /// Bound recursive operand, evaluated against the synthetic relation.
    pub step: SqlCatalogBoundSelect,
    /// Whether the body combined its operands with `UNION ALL` (keep duplicates).
    pub union_all: bool,
}

/// One ORDER BY item after SQL names are bound to 集群 catalog identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundOrderByItem {
    /// Ordering expression.
    pub expression: SqlCatalogBoundExpression,
    /// Sort direction.
    pub direction: SqlBoundOrderDirection,
    /// Explicit `NULLS FIRST` / `NULLS LAST`, when written.
    pub nulls: Option<SqlNullsOrder>,
}

/// One `OVER (...)` clause after SQL names are bound to 集群 catalog identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundWindowSpec {
    /// `PARTITION BY` expressions, in source order.
    pub partition_by: Vec<SqlCatalogBoundExpression>,
    /// Window `ORDER BY` items, in source order.
    pub order_by: Vec<SqlCatalogBoundOrderByItem>,
    /// Explicit `ROWS ...` frame, when one was written.
    pub frame: Option<SqlBoundWindowFrame>,
}

/// One JOIN clause after SQL names are bound to 集群 catalog identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundJoin {
    /// Join kind retained from the SQL surface.
    pub kind: SqlBoundJoinKind,
    /// Joined table identity.
    pub table: SqlCatalogBoundTable,
    /// Join condition bound against the tables visible at this point.
    pub on: SqlCatalogBoundExpression,
}

/// SQL INSERT request after catalog binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundInsert {
    /// Target table identity.
    pub table: SqlCatalogBoundTable,
    /// Catalog fields in the order VALUES supplies them.
    pub fields: Vec<SqlCatalogBoundColumn>,
    /// Bound value expressions grouped by row. Every inner vec has length `fields.len()`.
    pub rows: Vec<Vec<SqlCatalogBoundExpression>>,
    /// Catalog-bound `INSERT ... SELECT` source. `None` for a `VALUES` insert.
    pub select: Option<Box<SqlCatalogBoundSelect>>,
    /// `ON CONFLICT` clause, when one was written.
    pub conflict: Option<SqlCatalogBoundConflict>,
    /// `RETURNING` projections, when one was written.
    pub returning: Option<Vec<SqlCatalogBoundProjection>>,
}

/// `ON CONFLICT` clause after catalog binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundConflict {
    /// Conflict target columns as written by the client.
    pub columns: Vec<String>,
    /// Action taken when the target row already exists.
    pub action: SqlCatalogBoundConflictAction,
}

/// Catalog-bound conflict action of an `ON CONFLICT` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlCatalogBoundConflictAction {
    /// `DO NOTHING` — skip the conflicting row.
    DoNothing,
    /// `DO UPDATE SET ... [WHERE ...]` — rewrite the conflicting row in place.
    DoUpdate {
        /// Target column → catalog-bound assignment expression.
        assignments: Vec<(SqlCatalogBoundColumn, SqlCatalogBoundExpression)>,
        /// Optional predicate restricting which conflicting rows are rewritten.
        predicate: Option<SqlCatalogBoundExpression>,
    },
}

/// SQL UPDATE request after catalog binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundUpdate {
    /// Target table identity.
    pub table: SqlCatalogBoundTable,
    /// Catalog field → bound value assignments in source order.
    pub assignments: Vec<(SqlCatalogBoundColumn, SqlCatalogBoundExpression)>,
    /// Resolved optional predicate.
    pub predicate: Option<SqlCatalogBoundExpression>,
}

/// SQL DELETE request after catalog binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundDelete {
    /// Target table identity.
    pub table: SqlCatalogBoundTable,
    /// Extra row sources of `DELETE ... USING ...`, each bound to its own catalog table
    /// occurrence; the predicate sees them together with the target.
    pub using: Vec<SqlCatalogBoundTable>,
    /// Resolved optional predicate.
    pub predicate: Option<SqlCatalogBoundExpression>,
    /// `RETURNING` projections over the deleted rows, when one was written.
    ///
    /// Like PostgreSQL they may only reference the target table (through its
    /// alias); `USING` columns are deliberately not in scope.
    pub returning: Option<Vec<SqlCatalogBoundProjection>>,
}

/// One operand of a catalog-bound SQL set operation.
///
/// The binder folds the flat chain into a precedence tree, so an operand is
/// either a plain `SELECT` binding or an `INTERSECT` group of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlCatalogBoundSetOperand {
    /// One catalog-bound `SELECT` operand.
    Select(Box<SqlCatalogBoundSelect>),
    /// An `INTERSECT` group produced by the precedence fold.
    SetOperation(Box<SqlCatalogBoundSetOperation>),
}

/// SQL set operation after catalog binding.
///
/// `operands` and `operators` describe one precedence level: `operators` holds
/// `operands.len() - 1` entries, each combining its two neighbours
/// left-associatively. The output column names come from the leftmost operand.
///
/// `ORDER BY` on a set operation references the combined output columns rather
/// than catalog columns, so it stays in surface form until the planner resolves
/// it against the leftmost operand's result shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogBoundSetOperation {
    /// Operands in left-to-right order.
    pub operands: Vec<SqlCatalogBoundSetOperand>,
    /// Operators between consecutive operands.
    pub operators: Vec<SqlBoundSetOperator>,
    /// `ORDER BY` applied to the combined result, resolved during planning.
    pub order_by: Option<Vec<SqlBoundOrderByItem>>,
    /// Optional `LIMIT` count expression applied to the combined result.
    pub limit: Option<SqlCatalogBoundExpression>,
    /// Optional `OFFSET` count expression applied to the combined result.
    pub offset: Option<SqlCatalogBoundExpression>,
}

/// Gateway-owned SQL request after names are bound to a 集群 catalog revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlCatalogBoundStatement {
    /// One catalog-bound SELECT request awaiting distributed planning.
    Select(SqlCatalogBoundSelect),
    /// One catalog-bound INSERT request awaiting planning.
    Insert(SqlCatalogBoundInsert),
    /// One catalog-bound UPDATE request awaiting planning.
    Update(SqlCatalogBoundUpdate),
    /// One catalog-bound DELETE request awaiting planning.
    Delete(SqlCatalogBoundDelete),
    /// One catalog-bound set operation awaiting planning.
    SetOperation(SqlCatalogBoundSetOperation),
}

/// Why a column reference may not reach the directly enclosing query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlCorrelatedBlock {
    /// The subquery sits in a write statement, where no enclosing row is in scope yet.
    WriteStatement,
    /// The subquery is nested inside another subquery; only one correlation level exists.
    Nested,
    /// The reference sits in a `FROM` clause, which is flattened onto the enclosing row.
    FromClause,
}

impl SqlCorrelatedBlock {
    /// Client-facing reason a correlated reference is rejected in this position.
    fn message(self) -> &'static str {
        match self {
            Self::WriteStatement => "SQL correlated subquery is not supported in a write statement",
            Self::Nested => "SQL correlated subquery supports a single nesting level only",
            Self::FromClause => "SQL correlated column cannot appear in a subquery FROM clause",
        }
    }
}

/// Which tables an expression may correlate against, and whether that is allowed.
///
/// A correlated subquery is re-executed once per enclosing row, so the enclosing
/// tables stay visible inside it. `blocked` records positions where correlation is
/// refused instead of silently reading a value from the wrong row.
#[derive(Debug, Clone, Default)]
struct SqlCorrelationScope {
    /// Tables of the directly enclosing query, addressable by a correlated subquery.
    outer: Vec<SqlCatalogBoundTable>,
    /// Reason enclosing columns are rejected here, when one applies.
    blocked: Option<SqlCorrelatedBlock>,
}

impl SqlCorrelationScope {
    /// Scope of a statement-level `SELECT`: no enclosing tables, correlation allowed.
    fn root() -> Self {
        Self::default()
    }

    /// Scope of a write statement: subqueries may not read the written row.
    fn write() -> Self {
        Self { outer: Vec::new(), blocked: Some(SqlCorrelatedBlock::WriteStatement) }
    }

    /// Scope of one subquery whose enclosing query exposes `sources`.
    fn subquery(&self, sources: &[SqlCatalogBoundTable]) -> Self {
        Self { outer: sources.to_vec(), blocked: self.blocked.or_else(|| (!self.outer.is_empty()).then_some(SqlCorrelatedBlock::Nested)) }
    }

    /// Scope of a `FROM` body: enclosing tables stay visible only to be rejected.
    fn from_clause(&self, sources: &[SqlCatalogBoundTable]) -> Self {
        let mut outer = self.outer.clone();
        outer.extend(sources.iter().cloned());
        Self { outer, blocked: Some(SqlCorrelatedBlock::FromClause) }
    }
}

/// Binds a parsed SQL surface to a specific catalog revision.
///
/// `@@`-prefixed references are rejected — use
/// [`bind_sql_catalog_with_variables`] on gateways that emulate a variable table.
pub fn bind_sql_catalog(
    source: &str,
    catalog: &SqlCatalog,
    expected_schema_version: u32,
    dialect: SqlDialect,
) -> Result<SqlCatalogBoundStatement, SqlFrontendError> {
    bind_sql_catalog_with_variables(source, catalog, expected_schema_version, &reject_system_variable, dialect)
}

/// Binds a parsed SQL surface against a catalog revision, resolving system variables.
///
/// `variables` receives the whole `@@[scope.]name` spelling written by the
/// client and returns the literal the variable evaluates to. Dialects without
/// system variables keep using [`bind_sql_catalog`], which always rejects them.
pub fn bind_sql_catalog_with_variables(
    source: &str,
    catalog: &SqlCatalog,
    expected_schema_version: u32,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
    dialect: SqlDialect,
) -> Result<SqlCatalogBoundStatement, SqlFrontendError> {
    if catalog.schema_version != expected_schema_version {
        return Err(internal_error(&format!(
            "SQL catalog schema version mismatch: expected {expected_schema_version}, found {}",
            catalog.schema_version
        )));
    }
    // `::<type>` 在 `data.text` 里只有词法没有节点，PostgreSQL 方言先降为 `CAST(...)`，
    // 其它方言直接拒绝，绝不静默丢掉转换和列名。
    let cast_rewritten = crate::pgsql::rewrite_double_colon_casts(source, dialect)?;
    let source = cast_rewritten.as_ref();
    // MySQL 的 `||` 是逻辑 OR 而不是拼接；本方言的拼接一律写 `CONCAT(...)`。
    if dialect == SqlDialect::Mysql && find_keyword_outside_literals(source, "||").is_some() {
        return Err(SqlFrontendError::syntax("the || operator is not supported in the MySQL dialect, write CONCAT(...)"));
    }
    // MySQL 的 `ON DUPLICATE KEY UPDATE` 没有冲突目标，`data.text` 也不认这段语法。
    // 在绑定前把它改写成等价的 `ON CONFLICT (<primary key>) DO UPDATE`，冲突
    // 目标由 catalog 的主键补齐；写入语义仍由 planner/executor 的同一套冲突路径执行。
    let rewritten = if dialect == SqlDialect::Mysql { rewrite_mysql_duplicate_key(source, catalog)? } else { None };
    let statement = bind_sql(rewritten.as_deref().unwrap_or(source), dialect)?;
    match statement {
        SqlBoundStatement::Select(select) => bind_select(select, catalog, variables).map(SqlCatalogBoundStatement::Select),
        SqlBoundStatement::Insert(insert) => bind_insert(insert, catalog, variables).map(SqlCatalogBoundStatement::Insert),
        SqlBoundStatement::Update(update) => bind_update(update, catalog, variables).map(SqlCatalogBoundStatement::Update),
        SqlBoundStatement::Delete(delete) => bind_delete(delete, catalog, variables).map(SqlCatalogBoundStatement::Delete),
        SqlBoundStatement::SetOperation(set) => bind_set_operation(set, catalog, variables).map(SqlCatalogBoundStatement::SetOperation),
    }
}

/// Rewrites MySQL `INSERT ... ON DUPLICATE KEY UPDATE` into the parsed
/// `ON CONFLICT (<primary key>) DO UPDATE SET` form.
///
/// MySQL's clause has no conflict target, so the target is filled from the
/// catalog primary key. The gateway's conflict path keys rows by primary key;
/// a duplicate on another unique column is handled by the executor's unique
/// conflict check. `VALUES (col)` is rewritten to `EXCLUDED.col`, the
/// equivalent proposal-row reference.
fn rewrite_mysql_duplicate_key(source: &str, catalog: &SqlCatalog) -> Result<Option<String>, SqlFrontendError> {
    const CLAUSE: &str = "ON DUPLICATE KEY UPDATE";
    let Some(index) = find_keyword_outside_literals(source, CLAUSE)
    else {
        return Ok(None);
    };
    let head = &source[..index];
    let tail = &source[index + CLAUSE.len()..];
    let table_name = insert_target_table(head)
        .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::Syntax, "ON DUPLICATE KEY UPDATE requires an INSERT statement".to_string()))?;
    let table = find_table(catalog, table_name)?;
    let catalog_table = catalog
        .tables
        .iter()
        .find(|candidate| candidate.type_id == table.type_id)
        .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::Internal, format!("catalog table `{table_name}` is not published")))?;
    let primary_key = catalog_table
        .fields
        .first()
        .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::Internal, format!("table `{table_name}` has no fields")))?;
    let assignments = rewrite_values_references(tail);
    Ok(Some(format!("{head} ON CONFLICT (`{}`) DO UPDATE SET {assignments}", primary_key.name)))
}

/// Case-insensitive keyword search that ignores single-quoted strings,
/// double-quoted identifiers and backtick identifiers.
fn find_keyword_outside_literals(source: &str, keyword: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let needle = keyword.as_bytes();
    let mut index = 0;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(active) => {
                if byte == b'\\' {
                    index += 2;
                    continue;
                }
                if byte == active {
                    if active != b'`' && bytes.get(index + 1) == Some(&active) {
                        index += 2;
                        continue;
                    }
                    quote = None;
                }
                index += 1;
            }
            None => {
                if byte == b'\'' || byte == b'"' || byte == b'`' {
                    quote = Some(byte);
                    index += 1;
                    continue;
                }
                if index + needle.len() <= bytes.len() && bytes[index..index + needle.len()].eq_ignore_ascii_case(needle) {
                    return Some(index);
                }
                index += 1;
            }
        }
    }
    None
}

/// Finds the table name of an `INSERT [IGNORE] [INTO] <name>` head.
fn insert_target_table(head: &str) -> Option<&str> {
    let mut rest = head.trim_start();
    let verb = take_word(rest)?;
    if !verb.eq_ignore_ascii_case("insert") {
        return None;
    }
    rest = rest[verb.len()..].trim_start();
    while let Some(word) = take_word(rest) {
        if word.eq_ignore_ascii_case("ignore") || word.eq_ignore_ascii_case("into") {
            rest = rest[word.len()..].trim_start();
            continue;
        }
        break;
    }
    // Qualify the target as `schema`.`table` and keep the last segment.
    let (mut table, consumed) = parse_identifier(rest)?;
    let mut rest = rest[consumed..].trim_start();
    while let Some(after_dot) = rest.strip_prefix('.') {
        let after = after_dot.trim_start();
        let (segment, consumed) = parse_identifier(after)?;
        table = segment;
        rest = after[consumed..].trim_start();
    }
    Some(table)
}

fn take_word(input: &str) -> Option<&str> {
    let end = input.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')).unwrap_or(input.len());
    (end > 0).then(|| &input[..end])
}

/// Parses a bare or backtick/double-quoted identifier, returning its text with
/// the quotes stripped and the number of input bytes consumed.
fn parse_identifier(input: &str) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    match bytes.first() {
        Some(&quote @ (b'`' | b'"')) => {
            let mut end = 1;
            while end < bytes.len() {
                if bytes[end] == quote {
                    if bytes.get(end + 1) == Some(&quote) {
                        end += 2;
                        continue;
                    }
                    return Some((&input[1..end], end + 1));
                }
                end += 1;
            }
            None
        }
        Some(_) => {
            let end = input.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')).unwrap_or(input.len());
            (end > 0).then(|| (&input[..end], end))
        }
        None => None,
    }
}

/// Rewrites `VALUES (col)` / `VALUES(col)` references to `EXCLUDED.col`.
fn rewrite_values_references(tail: &str) -> String {
    let mut out = String::with_capacity(tail.len());
    let mut index = 0;
    while index < tail.len() {
        let Some(relative) = find_keyword_outside_literals(&tail[index..], "VALUES")
        else {
            out.push_str(&tail[index..]);
            break;
        };
        let start = index + relative;
        out.push_str(&tail[index..start]);
        let after_keyword = start + "VALUES".len();
        let rest = &tail[after_keyword..];
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('(') {
            out.push_str("VALUES");
            index = after_keyword;
            continue;
        }
        let open = after_keyword + (rest.len() - trimmed.len());
        let Some(close) = matching_paren(tail, open)
        else {
            out.push_str(&tail[start..]);
            break;
        };
        let inner = tail[open + 1..close].trim();
        if is_column_identifier(inner) {
            out.push_str("EXCLUDED.");
            out.push_str(inner);
        }
        else {
            out.push_str(&tail[start..=close]);
        }
        index = close + 1;
    }
    out
}

fn matching_paren(input: &str, open: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut index = open;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(active) => {
                if byte == b'\\' {
                    index += 2;
                    continue;
                }
                if byte == active {
                    quote = None;
                }
            }
            None => {
                if byte == b'\'' || byte == b'"' || byte == b'`' {
                    quote = Some(byte);
                }
                else if byte == b'(' {
                    depth += 1;
                }
                else if byte == b')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
            }
        }
        index += 1;
    }
    None
}

fn is_column_identifier(text: &str) -> bool {
    let inner = text
        .strip_prefix('`')
        .and_then(|text| text.strip_suffix('`'))
        .or_else(|| text.strip_prefix('"').and_then(|text| text.strip_suffix('"')))
        .unwrap_or(text);
    !inner.is_empty() && inner.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

/// Default resolver for dialects without `@@` system variables.
fn reject_system_variable(name: &str) -> Result<SqlBoundLiteral, SqlFrontendError> {
    Err(SqlFrontendError::new(SqlErrorKind::UnknownVariable, format!("SQL system variable {name} is not supported by this gateway")))
}

fn bind_insert(
    insert: SqlBoundInsert,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundInsert, SqlFrontendError> {
    let table = find_table(catalog, &insert.table)?;
    let source = [table.clone()];
    // 写语句没有「当前外层行」，子查询引用目标表列会读到错误行，绑定期就拒绝。
    let scope = SqlCorrelationScope::write();
    // `EXCLUDED` 是 `DO UPDATE` 里提议写入的那一行：同一个 catalog 表占用第二个
    // 行槽，它的列因此带上 `ROW_ID_STRIDE` 偏移，与既有行区分开。既有行仍是槽 0。
    let proposed = SqlCatalogBoundTable { alias: Some("EXCLUDED".to_string()), row_slot: 1, ..table.clone() };
    let bound_fields = match insert.columns {
        Some(cols) => cols.iter().map(|name| find_field(name, &source, catalog)).collect::<Result<Vec<_>, _>>()?,
        None => {
            // INSERT INTO t VALUES (...) 无列名 → 按 catalog 字段顺序
            let cat = catalog
                .tables
                .iter()
                .find(|c| c.type_id == table.type_id)
                .ok_or_else(|| internal_error("catalog table identity is not published"))?;
            cat.fields
                .iter()
                .map(|f| {
                    reject_unknown_field(f)?;
                    Ok(SqlCatalogBoundColumn {
                        type_id: table.type_id,
                        field_id: f.field_id,
                        row_id: f.field_id,
                        table_name: table.name.clone(),
                        field_name: f.name.clone(),
                        visible_name: f.name.clone(),
                        capacity: f.capacity,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let field_count = bound_fields.len();
    if field_count == 0 {
        return Err(SqlFrontendError::syntax("INSERT has no target columns".to_string()));
    }
    // `ON CONFLICT` 目标列保持表层拼写，唯一约束校验由 planner 完成；`DO UPDATE`
    // 赋值与 `WHERE` 在目标表作用域内绑定，因此引用了未知列会显式报错。
    let conflict = match insert.conflict {
        None => None,
        Some(conflict) => Some(SqlCatalogBoundConflict {
            columns: conflict.columns,
            action: match conflict.action {
                SqlBoundConflictAction::DoNothing => SqlCatalogBoundConflictAction::DoNothing,
                SqlBoundConflictAction::DoUpdate { assignments, predicate } => {
                    // 赋值右值可以引用既有行（槽 0，表名限定）与提议行（`EXCLUDED`）。
                    let update_scope = [table.clone(), proposed];
                    let bound_assignments = assignments
                        .into_iter()
                        .map(|assignment| {
                            Ok((
                                find_field(&assignment.column, &source, catalog)?,
                                bind_expression(canonicalize_excluded(assignment.value), &update_scope, &[], &scope, catalog, variables)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, SqlFrontendError>>()?;
                    SqlCatalogBoundConflictAction::DoUpdate {
                        assignments: bound_assignments,
                        // `WHERE` 仍只对既有行求值，因此看不到 `EXCLUDED` 的列。
                        predicate: predicate
                            .map(|predicate| bind_expression(predicate, &source, &[], &scope, catalog, variables))
                            .transpose()?,
                    }
                }
            },
        }),
    };
    let returning = match insert.returning {
        None => None,
        Some(projections) => {
            let mut bound = Vec::new();
            for projection in projections {
                bound.extend(bind_projection(projection, &source, &[], &scope, catalog, variables)?);
            }
            Some(bound)
        }
    };
    // `INSERT ... SELECT` 复用既有 SELECT 绑定；源查询的列数/类型在 planner 层对齐。
    // `DEFAULT VALUES` 的语义是「按列默认值生成一行」，但网关的写入路径从不按
    // 列默认值补值（`SqlSchemaRegistry` 里的 default 只用于元数据展示），
    // 因此这里结构化地拒绝，而不是伪造一行 `NULL`。
    if let SqlBoundInsertSource::DefaultValues = insert.source {
        return Err(SqlFrontendError::unsupported(
            "`INSERT ... DEFAULT VALUES` is not supported: the cluster gateway does not apply column defaults on insert, so there is no default row to write",
        ));
    }
    if let SqlBoundInsertSource::Select(select) = insert.source {
        let select = bind_select(*select, catalog, variables)?;
        return Ok(SqlCatalogBoundInsert {
            table,
            fields: bound_fields,
            rows: Vec::new(),
            select: Some(Box::new(select)),
            conflict,
            returning,
        });
    }
    if insert.rows.is_empty() {
        return Err(SqlFrontendError::syntax("INSERT has no VALUES".to_string()));
    }
    let mut rows = Vec::with_capacity(insert.rows.len());
    for row in insert.rows {
        if row.len() != field_count {
            return Err(SqlFrontendError::syntax(format!("INSERT column/value count mismatch: {} columns, {} values", field_count, row.len())));
        }
        let bound_row =
            row.iter().map(|v| bind_expression(v.clone(), &source, &[], &scope, catalog, variables)).collect::<Result<Vec<_>, _>>()?;
        rows.push(bound_row);
    }
    Ok(SqlCatalogBoundInsert { table, fields: bound_fields, rows, select: None, conflict, returning })
}

/// Strips one level of identifier quoting, turning `"user_id"` or `` `user_id` ``
/// into `user_id` and unescaping doubled quotes. Bare names pass through.
fn unquote_ident(name: &str) -> String {
    let trimmed = name.trim();
    let bytes = trimmed.as_bytes();
    if trimmed.len() >= 2 {
        let open = bytes[0];
        let close = bytes[trimmed.len() - 1];
        if (open == b'"' && close == b'"') || (open == b'`' && close == b'`') {
            let inner = &trimmed[1..trimmed.len() - 1];
            return inner.replace("\"\"", "\"").replace("``", "`");
        }
    }
    trimmed.to_owned()
}

/// Normalizes the `EXCLUDED` qualifier of an `ON CONFLICT DO UPDATE` value.
///
/// `EXCLUDED` is a keyword rather than a table name, and PostgreSQL folds
/// unquoted identifiers to lower case, so clients almost always write
/// `excluded.column`. The gateway matches identifiers case-sensitively, so the
/// qualifier is folded onto the sentinel spelling here and left untouched
/// everywhere else.
fn canonicalize_excluded(expression: SqlBoundExpression) -> SqlBoundExpression {
    use SqlBoundExpression::*;
    match expression {
        Identifier(name) => match name.split_once('.') {
            // `EXCLUDED."user_id"` is legal PostgreSQL and is what a quoted
            // writer emits, so the qualifier and column are unquoted before the
            // sentinel spelling is rebuilt. Matching the raw quoted text would
            // miss the catalog field.
            Some((qualifier, column)) if unquote_ident(qualifier).eq_ignore_ascii_case("excluded") => {
                Identifier(format!("EXCLUDED.{}", unquote_ident(column)))
            }
            _ => Identifier(name),
        },
        Binary { left, operator, right } => {
            Binary { left: Box::new(canonicalize_excluded(*left)), operator, right: Box::new(canonicalize_excluded(*right)) }
        }
        Unary { operator, expression } => Unary { operator, expression: Box::new(canonicalize_excluded(*expression)) },
        Function { name, args, distinct, window, filter } => Function {
            name,
            args: args.into_iter().map(canonicalize_excluded).collect(),
            distinct,
            window,
            filter: filter.map(|predicate| Box::new(canonicalize_excluded(*predicate))),
        },
        InSubquery { expression, select, negated } => InSubquery { expression: Box::new(canonicalize_excluded(*expression)), select, negated },
        Exists { select, negated } => Exists { select, negated },
        // 写语句禁止相关子查询，所以这里的子查询不会把外层列当作 `EXCLUDED` 别名，
        // 折叠无内容可改。
        ScalarSubquery { select } => ScalarSubquery { select },
        Case { branches, else_branch } => Case {
            branches: branches
                .into_iter()
                .map(|(when, then)| (Box::new(canonicalize_excluded(*when)), Box::new(canonicalize_excluded(*then))))
                .collect(),
            else_branch: else_branch.map(|els| Box::new(canonicalize_excluded(*els))),
        },
        Cast { expr, target } => Cast { expr: Box::new(canonicalize_excluded(*expr)), target },
        Literal(literal) => Literal(literal),
        Interval { text } => Interval { text },
        Wildcard => Wildcard,
    }
}

fn bind_update(
    update: SqlBoundUpdate,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundUpdate, SqlFrontendError> {
    let table = find_table(catalog, &update.table)?;
    let source = [table.clone()];
    let scope = SqlCorrelationScope::write();
    let assignments = update
        .assignments
        .into_iter()
        .map(|SqlBoundAssignment { column, value }| {
            let field = find_field(&column, &source, catalog)?;
            let bound_value = bind_expression(value, &source, &[], &scope, catalog, variables)?;
            Ok((field, bound_value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let predicate = update.predicate.map(|p| bind_expression(p, &source, &[], &scope, catalog, variables)).transpose()?;
    Ok(SqlCatalogBoundUpdate { table, assignments, predicate })
}

fn bind_delete(
    delete: SqlBoundDelete,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundDelete, SqlFrontendError> {
    let mut table = find_table(catalog, &delete.table)?;
    // DELETE 的目标表别名与 `USING` 源一起构成谓词的可见作用域：限定列 `x.col`
    // 按别名（无别名时按表名）解析；`USING` 表占用后续行槽，与 SELECT 连接同一套行值键规则。
    table.alias = delete.alias.clone();
    let mut occurrences = std::collections::BTreeMap::from([(table.type_id, 0u64)]);
    let mut using = Vec::with_capacity(delete.using.len());
    for (name, alias) in delete.using {
        let mut source = find_table(catalog, &name)?;
        source.alias = alias;
        using.push(assign_row_slot(source, &mut occurrences));
    }
    let mut sources = vec![table.clone()];
    sources.extend(using.iter().cloned());
    let scope = SqlCorrelationScope::write();
    let predicate = delete.predicate.map(|p| bind_expression(p, &sources, &[], &scope, catalog, variables)).transpose()?;
    // `RETURNING` 只看见目标表（含其别名），与 PostgreSQL 口径一致；
    // `USING` 列若出现在 RETURNING 里会在此处显式绑定失败。
    let target_source = [table.clone()];
    let returning = match delete.returning {
        None => None,
        Some(projections) => {
            let mut bound = Vec::new();
            for projection in projections {
                bound.extend(bind_projection(projection, &target_source, &[], &scope, catalog, variables)?);
            }
            Some(bound)
        }
    };
    Ok(SqlCatalogBoundDelete { table, using, predicate, returning })
}

fn bind_set_operation(
    set: SqlBoundSetOperation,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundSetOperation, SqlFrontendError> {
    bind_set_operation_in(set, &[], &SqlCorrelationScope::root(), catalog, variables)
}

/// Binds one set-operation level whose operands share the inherited CTE scope.
///
/// `WITH` written in front of a set operation covers the whole operation, so
/// its definitions are lifted off the leftmost leaf and every operand inherits
/// them.
fn bind_set_operation_in(
    set: SqlBoundSetOperation,
    inherited: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundSetOperation, SqlFrontendError> {
    let SqlBoundSetOperation { mut operands, operators, order_by, limit, offset } = set;
    if operands.len() < 2 || operators.len() + 1 != operands.len() {
        return Err(SqlFrontendError::new(SqlErrorKind::Syntax, "set operation operands and operators do not line up"));
    }
    let own_with = take_leading_with(&mut operands);
    let (ctes, declared, _augmented) = resolve_cte_scope(own_with, inherited, catalog, variables, scope)?;
    // The fixed point is materialized around the single SELECT that references it.
    // A set operation has no such host: each operand plans independently, so the
    // working set could not outlive the operand that declared it.
    if let Some(recursive) = declared.first() {
        return Err(SqlFrontendError::unsupported(format!(
            "recursive CTE `{}` cannot scope a top-level UNION through the SQL gateway: reference it inside one SELECT instead",
            recursive.name
        )));
    }
    let limit = bind_row_count(limit, &ctes, catalog, variables)?;
    let offset = bind_row_count(offset, &ctes, catalog, variables)?;
    let operands =
        operands.into_iter().map(|operand| bind_set_operand(operand, &ctes, scope, catalog, variables)).collect::<Result<Vec<_>, _>>()?;
    Ok(SqlCatalogBoundSetOperation { operands, operators, order_by, limit, offset })
}

fn bind_set_operand(
    operand: SqlBoundSetOperand,
    ctes: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundSetOperand, SqlFrontendError> {
    match operand {
        SqlBoundSetOperand::Select(select) => {
            bind_select_in(*select, catalog, variables, ctes, scope).map(|select| SqlCatalogBoundSetOperand::Select(Box::new(select)))
        }
        SqlBoundSetOperand::SetOperation(set) => {
            bind_set_operation_in(*set, ctes, scope, catalog, variables).map(|set| SqlCatalogBoundSetOperand::SetOperation(Box::new(set)))
        }
    }
}

/// Removes the leading `WITH` clause from the leftmost operand of a level.
///
/// The parser attaches `WITH` to the first written `SELECT`, so the definition
/// list that scopes the whole set operation is the one on the leftmost leaf.
fn take_leading_with(operands: &mut [SqlBoundSetOperand]) -> Vec<SqlBoundCte> {
    let Some(first) = operands.first_mut()
    else {
        return Vec::new();
    };
    match first {
        SqlBoundSetOperand::Select(select) => std::mem::take(&mut select.with),
        SqlBoundSetOperand::SetOperation(set) => take_leading_with(&mut set.operands),
    }
}

fn bind_select(
    select: SqlBoundSelect,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundSelect, SqlFrontendError> {
    bind_select_in(select, catalog, variables, &[], &SqlCorrelationScope::root())
}

/// Resolves the `WITH` definitions of one select into the scope its `FROM` sees,
/// keeping the definitions inherited from an enclosing scope.
///
/// Definitions apply in source order: each body only sees the definitions written
/// before it, so a non-recursive body can neither reference itself nor a later CTE,
/// and a name defined here shadows an inherited one. Every non-recursive definition
/// is flattened as soon as it is resolved, even when nothing references it, so an
/// unexpressible body never hides behind an unused name.
///
/// A recursive definition is materialized, not inlined: the seed binds against the
/// catalog as it stands, its projection names and types become a synthetic relation,
/// and the step binds against that relation made visible to itself. The synthetic
/// table is appended to an owned copy of the catalog which the caller uses for the
/// rest of this select, so column lookups by `type_id` find it like any catalog
/// table. The returned declarations travel with the bound select to the planner.
fn resolve_cte_scope(
    own: Vec<SqlBoundCte>,
    inherited: &[SqlCatalogCte],
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
    scope: &SqlCorrelationScope,
) -> Result<(Vec<SqlCatalogCte>, Vec<SqlCatalogRecursiveCte>, Option<SqlCatalog>), SqlFrontendError> {
    let mut ctes: Vec<SqlCatalogCte> = inherited.to_vec();
    let mut declared: Vec<SqlCatalogRecursiveCte> = Vec::new();
    let mut augmented: Option<SqlCatalog> = None;
    for cte in own {
        let catalog_now: &SqlCatalog = augmented.as_ref().unwrap_or(catalog);
        let Some(step) = cte.step else {
            let definition = SqlCatalogCte {
                name: cte.name.clone(),
                columns: cte.columns,
                query: *cte.query,
                visible: ctes.clone(),
                recursive: None,
            };
            let mut probe = std::collections::BTreeMap::new();
            flatten_body(
                &definition.query,
                &definition.name,
                &definition.columns,
                "common table expression",
                &definition.visible,
                &scope.from_clause(&[]),
                catalog_now,
                variables,
                &mut probe,
            )?;
            ctes.push(definition);
            continue;
        };
        // The seed never sees the synthetic relation: like PostgreSQL, only the
        // recursive operand may reference the CTE being defined.
        let seed_raw = *cte.query;
        let seed = bind_select_in(seed_raw.clone(), catalog_now, variables, &ctes, scope)?;
        let (names, field_types) = recursive_output_shape(&seed, &cte.columns, catalog_now)?;
        let seq = SYNTHETIC_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let type_id = u64::MAX - 1 - seq;
        let fields: Vec<SqlCatalogField> = names
            .iter()
            .zip(field_types)
            .enumerate()
            .map(|(index, (name, field_type))| SqlCatalogField {
                field_id: SYNTHETIC_FIELD_ID_TOP + seq * SYNTHETIC_FIELD_STRIDE + (index as u64) + 1,
                name: name.clone(),
                field_type,
                vos_type_name: None,
                capacity: None,
                default: None,
                check: None,
                generated: None,
            })
            .collect();
        let payload = SqlCatalogRecursiveCte { name: cte.name.clone(), type_id, fields: fields.clone(), seed, step: SqlCatalogBoundSelect {
            source: None,
            joins: Vec::new(),
            projections: Vec::new(),
            predicate: None,
            group_by: None,
            having: None,
            limit: None,
            offset: None,
            order_by: None,
            distinct: false,
            recursive_ctes: Vec::new(),
        }, union_all: cte.union_all };
        // The step sees the definition itself: its references to the CTE name scan
        // the synthetic relation, which is what makes the body a fixed point. The
        // synthetic table must already live in the catalog the step binds against,
        // because column lookup resolves its fields by `type_id`.
        let synthetic = SqlCatalogTable { type_id, name: cte.name.clone(), fields: fields.clone(), unique_constraints: Vec::new(), indexes: Vec::new(), check_constraints: Vec::new(), foreign_keys: Vec::new() };
        let mut tables = catalog_now.tables.clone();
        tables.push(synthetic);
        let augmented_now = SqlCatalog::assemble(catalog_now.schema_version, tables);
        let mut step_visible = ctes.clone();
        step_visible.push(SqlCatalogCte { name: cte.name.clone(), columns: cte.columns.clone(), query: seed_raw, visible: ctes.clone(), recursive: Some(Box::new(payload.clone())) });
        let step_bound = bind_select_in(*step, &augmented_now, variables, &step_visible, scope)?;
        let mut payload = payload;
        payload.step = step_bound;
        augmented = Some(augmented_now);
        declared.push(payload.clone());
        ctes.push(SqlCatalogCte { name: cte.name, columns: cte.columns, query: seed_raw_placeholder(), visible: ctes.clone(), recursive: Some(Box::new(payload)) });
    }
    Ok((ctes, declared, augmented))
}

/// Placeholder body kept on a recursive `SqlCatalogCte`.
///
/// Recursive definitions are never flattened, so this body is never read; it only
/// keeps the shared `SqlCatalogCte` shape uniform.
fn seed_raw_placeholder() -> SqlBoundSelect {
    SqlBoundSelect {
        with: Vec::new(),
        source: None,
        joins: Vec::new(),
        projections: Vec::new(),
        predicate: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        order_by: None,
        distinct: false,
    }
}

/// Output column names and types of a recursive CTE's seed.
///
/// Names come from the explicit `WITH name (a, b)` list when written, otherwise
/// from the seed projection's own labels. Types are inferred with the same
/// projection-type machinery `Describe` uses; a column whose seed type cannot be
/// inferred is rejected instead of becoming an untyped working-set column.
/// Every synthetic column is nullable: a later round may produce `NULL` where the
/// seed produced a value, and PostgreSQL types recursive columns by the seed
/// while still admitting `NULL` rows.
fn recursive_output_shape(
    seed: &SqlCatalogBoundSelect,
    columns: &[String],
    catalog: &SqlCatalog,
) -> Result<(Vec<String>, Vec<SqlFieldType>), SqlFrontendError> {
    let mut field_types = std::collections::BTreeMap::new();
    for table in seed.source.iter().chain(seed.joins.iter().map(|join| &join.table)) {
        let catalog_table = catalog
            .tables
            .iter()
            .find(|candidate| candidate.type_id == table.type_id)
            .ok_or_else(|| internal_error("catalog table identity is not published"))?;
        for field in &catalog_table.fields {
            field_types.insert(table.row_id(field.field_id), field.field_type.clone());
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut types: Vec<SqlFieldType> = Vec::new();
    for projection in &seed.projections {
        match projection {
            SqlCatalogBoundProjection::Star(table) => {
                let catalog_table = catalog
                    .tables
                    .iter()
                    .find(|candidate| candidate.type_id == table.type_id)
                    .ok_or_else(|| internal_error("catalog table identity is not published"))?;
                for (index, field) in catalog_table.fields.iter().enumerate() {
                    reject_unknown_field(field)?;
                    if !table.exposes_column(index) {
                        continue;
                    }
                    names.push(table.visible_column_name(index, field));
                    types.push(field.field_type.clone());
                }
            }
            SqlCatalogBoundProjection::Expression { expression, alias } => {
                let inferred = crate::sql_describe::infer_expression_type(expression, &field_types, SqlDialect::Postgres);
                if inferred.inner() == SqlFieldType::Unknown {
                    let label = alias.clone().unwrap_or_else(|| crate::sql_planner::expression_default_label(expression));
                    return Err(SqlFrontendError::unsupported(format!(
                        "recursive CTE column `{label}` has no type the seed projection can establish: give the seed a typed column or an explicit cast"
                    )));
                }
                names.push(alias.clone().unwrap_or_else(|| match expression {
                    SqlCatalogBoundExpression::Column(column) => column.visible_name.clone(),
                    other => crate::sql_planner::expression_default_label(other),
                }));
                types.push(match inferred {
                    SqlFieldType::Optional(_) => inferred,
                    inner => SqlFieldType::Optional(Box::new(inner)),
                });
            }
        }
    }
    if names.is_empty() {
        return Err(SqlFrontendError::unsupported("a recursive CTE seed must project at least one column"));
    }
    if !columns.is_empty() {
        if columns.len() != names.len() {
            return Err(unsupported_source(
                "common table expression",
                &format!("the explicit column list names {} columns but the seed outputs {}", columns.len(), names.len()),
            ));
        }
        names = columns.to_vec();
    }
    if names.len() as u64 > SYNTHETIC_FIELD_STRIDE {
        return Err(SqlFrontendError::unsupported(format!(
            "a recursive CTE cannot expose more than {SYNTHETIC_FIELD_STRIDE} columns through the SQL gateway"
        )));
    }
    Ok((names, types))
}

/// Binds one select whose `FROM` may also reference CTEs from an enclosing scope.
fn bind_select_in(
    select: SqlBoundSelect,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
    inherited: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
) -> Result<SqlCatalogBoundSelect, SqlFrontendError> {
    // CTE 在引用处才被展开成基表，同一条 CTE 被引用两次就得到两个互不覆盖的行槽。
    let (ctes, declared, augmented) = resolve_cte_scope(select.with, inherited, catalog, variables, scope)?;
    // 递归 CTE 的合成表随增强目录一起参与本 select 的列解析；没有递归定义时
    // 增强目录为空，沿用调用方传入的目录。
    let catalog: &SqlCatalog = augmented.as_ref().unwrap_or(catalog);
    let mut occurrences: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
    // 派生表被扁平化成它自己的基表，内层 `WHERE` 改由外层 `WHERE` 表达。
    let mut derived_predicates: Vec<SqlCatalogBoundExpression> = Vec::new();
    let mut source = None;
    // FROM 里的表在解析时还不构成「外层行」，因此这里只把外层表传给 FROM 以显式拒绝。
    if let Some(bound_source) = select.source.as_ref() {
        let (table, predicate) = bind_source(bound_source, catalog, variables, &mut occurrences, &ctes, &scope.from_clause(&[]))?;
        source = Some(table);
        if let Some(predicate) = predicate {
            // 保留侧的过滤移动到外层 `WHERE` 语义不变；可空侧会因 NULL 扩张丢行。
            if select.joins.iter().any(|join| matches!(join.kind, SqlBoundJoinKind::Right | SqlBoundJoinKind::Full)) {
                return Err(unsupported_source("derived table", "a filtered source cannot feed a RIGHT or FULL join"));
            }
            derived_predicates.push(predicate);
        }
    }
    // JOIN 之后列名作用域包含基表与所有已连接表，ON 条件只能引用此前已出现的表。
    let mut sources: Vec<SqlCatalogBoundTable> = source.iter().cloned().collect();
    let mut joins: Vec<SqlCatalogBoundJoin> = Vec::with_capacity(select.joins.len());
    for join in select.joins {
        let (table, derived_predicate) = bind_source(&join.source, catalog, variables, &mut occurrences, &ctes, &scope.from_clause(&sources))?;
        if let Some(predicate) = derived_predicate {
            if !matches!(join.kind, SqlBoundJoinKind::Inner) {
                return Err(unsupported_source("derived table", "a filtered source can only feed an INNER join"));
            }
            derived_predicates.push(predicate);
        }
        if let Some(previous) = sources.iter().find(|visible| visible.visible_name() == table.visible_name()) {
            return Err(SqlFrontendError::new(
                SqlErrorKind::AmbiguousTable,
                format!("SQL table name `{}` appears more than once in FROM, give each occurrence a distinct alias", previous.visible_name()),
            ));
        }
        sources.push(table.clone());
        let on = bind_expression(join.on, &sources, &ctes, scope, catalog, variables)?;
        joins.push(SqlCatalogBoundJoin { kind: join.kind, table, on });
    }
    let mut projections: Vec<SqlCatalogBoundProjection> = Vec::new();
    for projection in select.projections {
        projections.extend(bind_projection(projection, &sources, &ctes, scope, catalog, variables)?);
    }
    let mut predicate =
        select.predicate.map(|expression| bind_expression(expression, &sources, &ctes, scope, catalog, variables)).transpose()?;
    for derived in derived_predicates {
        predicate = merge_predicates(predicate, Some(derived));
    }
    let group_by = select
        .group_by
        .map(|exprs| exprs.into_iter().map(|e| bind_expression(e, &sources, &ctes, scope, catalog, variables)).collect::<Result<Vec<_>, _>>())
        .transpose()?;
    let having = select.having.map(|e| bind_expression(e, &sources, &ctes, scope, catalog, variables)).transpose()?;
    let order_by = match select.order_by {
        Some(items) => {
            let bound = items
                .into_iter()
                .map(|item| {
                    Ok(SqlCatalogBoundOrderByItem {
                        expression: bind_expression(item.expr, &sources, &ctes, scope, catalog, variables)?,
                        direction: item.direction,
                        nulls: item.nulls,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Some(bound)
        }
        None => None,
    };
    Ok(SqlCatalogBoundSelect {
        source,
        joins,
        projections,
        predicate,
        group_by,
        having,
        limit: bind_row_count(select.limit, &ctes, catalog, variables)?,
        offset: bind_row_count(select.offset, &ctes, catalog, variables)?,
        order_by,
        distinct: select.distinct,
        recursive_ctes: declared,
    })
}

/// Binds one SQL projection into zero or more catalog projections (`*` expands one
/// entry per visible table so joined queries project both sides in source order).
fn bind_projection(
    projection: SqlBoundProjection,
    sources: &[SqlCatalogBoundTable],
    ctes: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<Vec<SqlCatalogBoundProjection>, SqlFrontendError> {
    match projection {
        SqlBoundProjection::Star => {
            if sources.is_empty() {
                return Err(SqlFrontendError::unsupported("SQL star projection requires a catalog table source"));
            }
            Ok(sources.iter().cloned().map(SqlCatalogBoundProjection::Star).collect())
        }
        SqlBoundProjection::Expression { expression, alias } => Ok(vec![SqlCatalogBoundProjection::Expression {
            expression: bind_expression(expression, sources, ctes, scope, catalog, variables)?,
            alias,
        }]),
    }
}

/// 收集「左结合 `OR` 链」里的全部等值项 `(左侧表达式, 右侧字面量)`。
///
/// 任一节点不是 `OR`、或某个等值项右侧不是字面量时返回 `false`，调用方保留原树。
fn collect_or_equalities(expression: &SqlBoundExpression, out: &mut Vec<(SqlBoundExpression, SqlBoundLiteral)>) -> bool {
    match expression {
        SqlBoundExpression::Binary { left, operator: SqlBoundBinaryOperator::Or, right } => {
            collect_or_equalities(left, out) && collect_or_equalities(right, out)
        }
        SqlBoundExpression::Binary { left, operator: SqlBoundBinaryOperator::Equal, right } => match right.as_ref() {
            SqlBoundExpression::Literal(value) => {
                out.push((left.as_ref().clone(), value.clone()));
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// Declared type of a bound column, resolved back through the catalog.
fn bound_column_type(column: &SqlCatalogBoundColumn, catalog: &SqlCatalog) -> Option<SqlFieldType> {
    let table = catalog.tables().iter().find(|table| table.type_id == column.type_id)?;
    let field = table.fields.iter().find(|field| field.field_id == column.field_id)?;
    Some(field.field_type.clone())
}

/// Rewrites a UUID literal into its canonical hyphenated lowercase spelling.
///
/// Stored uuid columns decode to that spelling, so comparisons, `LIKE` and `IN`
/// must see the same text; otherwise `WHERE id = 'UPPER-CASE-UUID'` would
/// compare two spellings of one value and miss the row. Text that is not a
/// UUID is rejected, matching typed `uuid` columns elsewhere.
fn normalize_uuid_literal(literal: &mut SqlBoundLiteral) -> Result<(), SqlFrontendError> {
    let SqlBoundLiteral::String(text) = literal
    else {
        return Ok(());
    };
    let bytes = parse_uuid(text).ok_or_else(|| SqlFrontendError::new(SqlErrorKind::TypeMismatch, format!("`{text}` is not a valid UUID")))?;
    *text = uuid_text(&bytes).expect("16 canonical octets always render as a UUID");
    Ok(())
}

/// Canonicalizes the literal side of any comparison that involves a uuid column.
fn normalize_uuid_comparison(
    left: &mut SqlCatalogBoundExpression,
    right: &mut SqlCatalogBoundExpression,
    catalog: &SqlCatalog,
) -> Result<(), SqlFrontendError> {
    if let SqlCatalogBoundExpression::Column(column) = &*left {
        if matches!(bound_column_type(column, catalog).map(|ty| ty.inner()), Some(SqlFieldType::Uuid)) {
            if let SqlCatalogBoundExpression::Literal(literal) = &mut *right {
                normalize_uuid_literal(literal)?;
            }
            return Ok(());
        }
    }
    if let SqlCatalogBoundExpression::Column(column) = &*right {
        if matches!(bound_column_type(column, catalog).map(|ty| ty.inner()), Some(SqlFieldType::Uuid)) {
            if let SqlCatalogBoundExpression::Literal(literal) = &mut *left {
                normalize_uuid_literal(literal)?;
            }
        }
    }
    Ok(())
}

fn bind_expression(
    expression: SqlBoundExpression,
    sources: &[SqlCatalogBoundTable],
    ctes: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundExpression, SqlFrontendError> {
    match expression {
        SqlBoundExpression::Identifier(name) => {
            // `@@` 前缀属于会话变量而不是 catalog 列名，必须先交给方言解析器。
            if name.starts_with("@@") {
                return variables(&name).map(SqlCatalogBoundExpression::Literal);
            }
            // 本层作用域解析不到时，再按「直接外层查询的列」找一次：相关子查询按外层行
            // 逐行重算，值不来自子查询自己的行。该位置禁止相关时在这里显式拒绝。
            let outcome = match find_field(&name, sources, catalog) {
                Ok(field) => Ok(SqlCatalogBoundExpression::Column(field)),
                Err(error) => match find_outer_column(&name, scope, catalog) {
                    Ok(Some(column)) => Ok(SqlCatalogBoundExpression::OuterColumn(column)),
                    Ok(None) => Err(error),
                    Err(error) => Err(error),
                },
            };
            match outcome {
                Ok(expression) => Ok(expression),
                // SQL 标准把 `CURRENT_DATE` / `CURRENT_TIME` / `CURRENT_TIMESTAMP` 写成
                // 无括号关键字，`data.text` 的树里没有对应节点。列解析失败后回退为无参内建函数，
                // 同名列优先，作用域与普通列引用一致。
                Err(error) if error.kind == SqlErrorKind::UnknownColumn => match SqlScalarFunction::from_keyword(&name) {
                    Some(function) => Ok(SqlCatalogBoundExpression::Function {
                        name: function.name().to_ascii_lowercase(),
                        args: Vec::new(),
                        distinct: false,
                        window: None,
                        filter: None,
                    }),
                    None => Err(error),
                },
                Err(error) => Err(error),
            }
        }
        SqlBoundExpression::Literal(literal) => Ok(SqlCatalogBoundExpression::Literal(literal)),
        SqlBoundExpression::Wildcard => Ok(SqlCatalogBoundExpression::Wildcard),
        SqlBoundExpression::Binary { left, operator, right } => {
            // 绑定阶段把 `expr IN (v1, …, vN)` 展开成左结合等值 `OR` 链。这里把
            // 「同一左侧、右侧全是字面量」的等值链还原成 `InValues`：逐行求值从
            // N 次比较降为一次集合匹配，也避免 N 较大时递归求值耗尽线程栈。
            if operator == SqlBoundBinaryOperator::Or {
                let chain = SqlBoundExpression::Binary { left: left.clone(), operator, right: right.clone() };
                let mut equalities = Vec::new();
                if collect_or_equalities(&chain, &mut equalities)
                    && equalities.len() > 1
                    && equalities.windows(2).all(|window| window[0].0 == window[1].0)
                {
                    let target = equalities[0].0.clone();
                    let mut values: Vec<SqlBoundLiteral> = equalities.into_iter().map(|(_, value)| value).collect();
                    let sorted_integers = crate::sql_execute::normalize_integer_values(&mut values);
                    let expression = bind_expression(target, sources, ctes, scope, catalog, variables)?;
                    // uuid 列与文本值比较时必须先规范化，否则 `IN` 会因为大小写/写法
                    // 不同而漏掉实际相等的行。
                    if let SqlCatalogBoundExpression::Column(column) = &expression {
                        if matches!(bound_column_type(column, catalog).map(|ty| ty.inner()), Some(SqlFieldType::Uuid)) {
                            for value in &mut values {
                                normalize_uuid_literal(value)?;
                            }
                        }
                    }
                    return Ok(SqlCatalogBoundExpression::InValues {
                        expression: Box::new(expression),
                        values,
                        negated: false,
                        sorted_integers,
                    });
                }
            }
            let mut bound_left = bind_expression(*left, sources, ctes, scope, catalog, variables)?;
            let mut bound_right = bind_expression(*right, sources, ctes, scope, catalog, variables)?;
            normalize_uuid_comparison(&mut bound_left, &mut bound_right, catalog)?;
            Ok(SqlCatalogBoundExpression::Binary { left: Box::new(bound_left), operator, right: Box::new(bound_right) })
        }
        SqlBoundExpression::Unary { operator, expression } => Ok(SqlCatalogBoundExpression::Unary {
            operator,
            expression: Box::new(bind_expression(*expression, sources, ctes, scope, catalog, variables)?),
        }),
        SqlBoundExpression::Function { name, args, distinct, window, filter } => {
            let bound_args =
                args.into_iter().map(|arg| bind_expression(arg, sources, ctes, scope, catalog, variables)).collect::<Result<Vec<_>, _>>()?;
            let window = window.map(|spec| bind_window_spec(spec, sources, ctes, scope, catalog, variables)).transpose()?;
            let filter =
                filter.map(|predicate| bind_expression(*predicate, sources, ctes, scope, catalog, variables).map(Box::new)).transpose()?;
            Ok(SqlCatalogBoundExpression::Function { name, args: bound_args, distinct, window, filter })
        }
        SqlBoundExpression::InSubquery { expression, select, negated } => {
            // 子查询自带 FROM 作用域，直接外层查询的列仍可见一层；外层 `WITH` 定义同样
            // 可见。执行期按外层行重算整个子查询，因此只有单层相关可以表达。
            let bound_select = bind_select_in(*select, catalog, variables, ctes, &scope.subquery(sources))?;
            reject_correlated_grouping(&bound_select)?;
            let plan = crate::sql_planner::plan_select(bound_select, catalog)?;
            Ok(SqlCatalogBoundExpression::InSubquery {
                expression: Box::new(bind_expression(*expression, sources, ctes, scope, catalog, variables)?),
                plan: Box::new(plan),
                negated,
            })
        }
        SqlBoundExpression::ScalarSubquery { select } => {
            // 标量子查询与 `EXISTS` 一样按外层行重算，外层 `WITH` 可见，外层列可见一层。
            // 它的值必须是单一列，因此在绑定期就拒绝多列投影，而不是在执行期近似取首列。
            let bound_select = bind_select_in(*select, catalog, variables, ctes, &scope.subquery(sources))?;
            reject_correlated_grouping(&bound_select)?;
            let plan = crate::sql_planner::plan_select(bound_select, catalog)?;
            if crate::sql_describe::describe_plan(&plan).len() != 1 {
                return Err(SqlFrontendError::unsupported("SQL scalar subquery must select exactly one column"));
            }
            Ok(SqlCatalogBoundExpression::ScalarSubquery { plan: Box::new(plan) })
        }
        SqlBoundExpression::Exists { select, negated } => {
            // `EXISTS` 与 `IN` 一样按外层行重算，外层 `WITH` 定义同样可见。
            let bound_select = bind_select_in(*select, catalog, variables, ctes, &scope.subquery(sources))?;
            reject_correlated_grouping(&bound_select)?;
            let plan = crate::sql_planner::plan_select(bound_select, catalog)?;
            Ok(SqlCatalogBoundExpression::Exists { plan: Box::new(plan), negated })
        }
        SqlBoundExpression::Case { branches, else_branch } => {
            let bound_branches = branches
                .into_iter()
                .map(|(when, then)| {
                    let w = bind_expression(*when, sources, ctes, scope, catalog, variables)?;
                    let t = bind_expression(*then, sources, ctes, scope, catalog, variables)?;
                    Ok((Box::new(w), Box::new(t)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let bound_else = match else_branch {
                Some(els) => Some(Box::new(bind_expression(*els, sources, ctes, scope, catalog, variables)?)),
                None => None,
            };
            Ok(SqlCatalogBoundExpression::Case { branches: bound_branches, else_branch: bound_else })
        }
        SqlBoundExpression::Cast { expr, target } => {
            Ok(SqlCatalogBoundExpression::Cast { expr: Box::new(bind_expression(*expr, sources, ctes, scope, catalog, variables)?), target })
        }
        SqlBoundExpression::Interval { text } => Ok(SqlCatalogBoundExpression::Interval { text }),
    }
}

/// Binds a `LIMIT` / `OFFSET` expression, which has no row context.
///
/// The gateway binder already rejected column references, `*` and subqueries
/// for these clauses, so binding runs against an empty scope. Keeping the
/// clause in the expression system is what lets the planner fold a parameter or
/// an arithmetic expression instead of a literal-only special case.
fn bind_row_count(
    expression: Option<SqlBoundExpression>,
    ctes: &[SqlCatalogCte],
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<Option<SqlCatalogBoundExpression>, SqlFrontendError> {
    expression.map(|expression| bind_expression(expression, &[], ctes, &SqlCorrelationScope::root(), catalog, variables)).transpose()
}

/// Resolves a column name against the tables of the directly enclosing query.
///
/// `Ok(None)` means the name belongs to neither the current scope nor the enclosing
/// one, so the caller keeps reporting the original unknown-column error. A match in
/// an enclosing scope that the current position forbids reports the reason instead,
/// which keeps `UnknownColumn` from hiding an explicitly unsupported form.
fn find_outer_column(name: &str, scope: &SqlCorrelationScope, catalog: &SqlCatalog) -> Result<Option<SqlCatalogOuterColumn>, SqlFrontendError> {
    if scope.outer.is_empty() {
        return Ok(None);
    }
    let column = match find_field(name, &scope.outer, catalog) {
        Ok(column) => column,
        Err(error) if matches!(error.kind, SqlErrorKind::UnknownColumn | SqlErrorKind::UnknownTableQualifier) => return Ok(None),
        Err(error) => return Err(error),
    };
    if let Some(blocked) = scope.blocked {
        return Err(SqlFrontendError::unsupported(blocked.message()));
    }
    let field_type = field_type_of(catalog, &column)?;
    Ok(Some(SqlCatalogOuterColumn { column, field_type }))
}

/// Looks up the declared type of a bound column through its catalog identity.
fn field_type_of(catalog: &SqlCatalog, column: &SqlCatalogBoundColumn) -> Result<SqlFieldType, SqlFrontendError> {
    let table = catalog
        .tables
        .iter()
        .find(|candidate| candidate.type_id == column.type_id)
        .ok_or_else(|| internal_error("catalog table identity is not published"))?;
    Ok(table
        .fields
        .iter()
        .find(|field| field.field_id == column.field_id)
        .map(|field| field.field_type.clone())
        .unwrap_or(SqlFieldType::Unknown))
}

/// Rejects a correlated subquery whose `GROUP BY` / `HAVING` / `ORDER BY` reads an
/// enclosing column.
///
/// The subquery is re-executed once per outer row, so those terms would be constant
/// for that execution. That is expressible, but keeping it unsupported is what
/// bounds the feature to the placements the planner can validate statically.
fn reject_correlated_grouping(select: &SqlCatalogBoundSelect) -> Result<(), SqlFrontendError> {
    let reads_outer = |clause: &str, hit: bool| {
        if hit { Err(SqlFrontendError::unsupported(format!("SQL correlated column cannot appear in a subquery {clause}"))) } else { Ok(()) }
    };
    reads_outer("GROUP BY", select.group_by.iter().flatten().any(crate::sql_planner::expression_reads_outer))?;
    reads_outer("HAVING", select.having.as_ref().is_some_and(crate::sql_planner::expression_reads_outer))?;
    reads_outer("ORDER BY", select.order_by.iter().flatten().any(|item| crate::sql_planner::expression_reads_outer(&item.expression)))
}

/// Catalog-binds one window spec, keeping partition / ordering terms resolvable
/// against the same table scope as the call itself.
fn bind_window_spec(
    spec: SqlBoundWindowSpec,
    sources: &[SqlCatalogBoundTable],
    ctes: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
) -> Result<SqlCatalogBoundWindowSpec, SqlFrontendError> {
    let partition_by = spec
        .partition_by
        .into_iter()
        .map(|expr| bind_expression(expr, sources, ctes, scope, catalog, variables))
        .collect::<Result<Vec<_>, _>>()?;
    let mut order_by = Vec::with_capacity(spec.order_by.len());
    for item in spec.order_by {
        order_by.push(SqlCatalogBoundOrderByItem {
            expression: bind_expression(item.expr, sources, ctes, scope, catalog, variables)?,
            direction: item.direction,
            nulls: item.nulls,
        });
    }
    Ok(SqlCatalogBoundWindowSpec { partition_by, order_by, frame: spec.frame })
}

/// One `WITH` definition kept while binding, together with the definitions
/// visible inside its body.
///
/// A CTE is never executed on its own: every reference flattens the body onto the
/// single catalog table it selects from, which is why the same CTE referenced
/// twice behaves like a self join. `visible` holds only the definitions written
/// before this one, so a body can neither reference itself nor a later CTE.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlCatalogCte {
    /// Name the CTE is referenced by.
    name: String,
    /// Explicit output column names, applied positionally to the flattened table.
    columns: Vec<String>,
    /// Bound body of the CTE.
    query: SqlBoundSelect,
    /// Definitions visible inside `query`, in source order.
    visible: Vec<SqlCatalogCte>,
    /// `Some` for a `WITH RECURSIVE` definition: references scan the synthetic
    /// relation instead of flattening `query` at each occurrence.
    recursive: Option<Box<SqlCatalogRecursiveCte>>,
}

/// Binds one `FROM` / `JOIN` source, assigning the occurrence slot used to build row ids.
///
/// Repeated occurrences of the same catalog table get distinct slots so a self join
/// can keep both sides of a column in one merged row. A CTE name shadows a catalog
/// table of the same name. A derived table or a CTE reference also returns the
/// predicate its body carried, for the caller to merge into the outer `WHERE`.
fn bind_source(
    source: &SqlBoundSource,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
    occurrences: &mut std::collections::BTreeMap<u64, u64>,
    ctes: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
) -> Result<(SqlCatalogBoundTable, Option<SqlCatalogBoundExpression>), SqlFrontendError> {
    match source {
        SqlBoundSource::Table { name, alias } => match ctes.iter().rev().find(|cte| cte.name == *name) {
            Some(cte) if cte.recursive.is_some() => Ok((recursive_occurrence(cte, alias.as_deref().unwrap_or(name), occurrences)?, None)),
            Some(cte) => flatten_body(
                &cte.query,
                alias.as_deref().unwrap_or(name),
                &cte.columns,
                "common table expression",
                &cte.visible,
                scope,
                catalog,
                variables,
                occurrences,
            ),
            None => {
                let mut table = find_table(catalog, name)?;
                table.alias = alias.clone();
                Ok((assign_row_slot(table, occurrences), None))
            }
        },
        SqlBoundSource::Derived { query, alias } => {
            // 派生表没有显式列名，只沿用 body 自己的输出名。
            flatten_body(query, alias, &[], "derived table", ctes, scope, catalog, variables, occurrences)
        }
    }
}

/// Assigns the next occurrence slot of one catalog table and returns the table.
fn assign_row_slot(mut table: SqlCatalogBoundTable, occurrences: &mut std::collections::BTreeMap<u64, u64>) -> SqlCatalogBoundTable {
    let slot = occurrences.entry(table.type_id).or_insert(0);
    table.row_slot = *slot;
    *slot += 1;
    table
}

/// Builds the single occurrence of a recursive CTE's synthetic relation.
///
/// The fixed point materializes into one working set keyed by the synthetic
/// field ids with row slot pinned to `0`, so its row ids equal its field ids.
/// A second reference to the same recursive CTE in one query — a self join of
/// the fixed point — cannot get its own row slot, so it is rejected instead of
/// silently sharing one.
fn recursive_occurrence(
    cte: &SqlCatalogCte,
    alias: &str,
    occurrences: &mut std::collections::BTreeMap<u64, u64>,
) -> Result<SqlCatalogBoundTable, SqlFrontendError> {
    let payload = cte.recursive.as_ref().ok_or_else(|| internal_error("recursive CTE occurrence without a bound payload"))?;
    let slot = occurrences.entry(payload.type_id).or_insert(0);
    if *slot != 0 {
        return Err(SqlFrontendError::unsupported(format!(
            "recursive CTE `{}` cannot be referenced more than once in one query: the gateway materializes a single working set for its fixed point",
            cte.name
        )));
    }
    *slot += 1;
    Ok(SqlCatalogBoundTable {
        type_id: payload.type_id,
        name: cte.name.clone(),
        alias: Some(alias.to_string()),
        row_slot: 0,
        column_names: None,
        visible_columns: None,
    })
}

/// Flattens one derived table or CTE body into the single catalog table it
/// selects from.
///
/// The gateway row source is one catalog table, so a body is expressible only
/// when it is that same table seen through a new name: no join, no grouping, no
/// ordering, no paging, no nested `WITH` and a projection that exposes every
/// column. Its source may also be an earlier CTE, which is flattened the same
/// way. The body's filter is returned to the caller, which ANDs it into the
/// outer `WHERE` — sound only while the columns are never NULL-extended by an
/// outer join, which the caller checks against the join kinds.
fn flatten_body(
    query: &SqlBoundSelect,
    alias: &str,
    columns: &[String],
    subject: &str,
    visible: &[SqlCatalogCte],
    scope: &SqlCorrelationScope,
    catalog: &SqlCatalog,
    variables: &dyn Fn(&str) -> Result<SqlBoundLiteral, SqlFrontendError>,
    occurrences: &mut std::collections::BTreeMap<u64, u64>,
) -> Result<(SqlCatalogBoundTable, Option<SqlCatalogBoundExpression>), SqlFrontendError> {
    if !query.joins.is_empty() {
        return Err(unsupported_source(subject, "its inner query joins more than one table"));
    }
    if !query.with.is_empty() {
        return Err(unsupported_source(subject, "its inner query declares its own WITH clause"));
    }
    for (present, clause) in [
        (query.group_by.is_some(), "GROUP BY"),
        (query.having.is_some(), "HAVING"),
        (query.distinct, "DISTINCT"),
        (query.order_by.is_some(), "ORDER BY"),
        (query.limit.is_some() || query.offset.is_some(), "LIMIT/OFFSET"),
    ] {
        if present {
            return Err(unsupported_source(subject, &format!("its inner query carries {clause}")));
        }
    }
    let Some(source) = query.source.as_ref()
    else {
        return Err(unsupported_source(subject, "its inner query has no table source"));
    };
    let SqlBoundSource::Table { name, alias: inner_alias } = source
    else {
        return Err(unsupported_source(subject, "its inner query selects from another derived table"));
    };
    // 内层来源既可以是 catalog 表，也可以是更早定义的 CTE；非递归保证这条链必然终止。
    // 更早 CTE 的显式列名同样作用于这里的内层展开，否则 `SELECT x FROM renamed_cte`
    // 会在扁平化时按基表字段名解析失败。
    let (table, inner_predicate) = match visible.iter().rev().find(|cte| cte.name == *name) {
        Some(cte) if cte.recursive.is_some() => (recursive_occurrence(cte, inner_alias.as_deref().unwrap_or(name), occurrences)?, None),
        Some(cte) => flatten_body(
            &cte.query,
            inner_alias.as_deref().unwrap_or(name),
            &cte.columns,
            "common table expression",
            &cte.visible,
            scope,
            catalog,
            variables,
            occurrences,
        )?,
        None => {
            let base = find_table(catalog, name)?;
            (assign_row_slot(SqlCatalogBoundTable { alias: inner_alias.clone(), ..base }, occurrences), None)
        }
    };
    // 行槽已在展开时确定，过滤条件必须绑定在这一份行值上：这样一条带过滤的 CTE
    // 与同一张表的另一次出现共存时，过滤不会落到别的行槽上。过滤使用 body 自己的
    // 列名作用域，显式输出列名只改外层看到的列名。
    let Some((field_indices, output_names)) = projected_output_columns(&query.projections, std::slice::from_ref(&table), catalog)?
    else {
        return Err(unsupported_source(subject, "its inner projection must be `*` or a column reference per output column"));
    };
    let predicate = query
        .predicate
        .clone()
        .map(|expression| {
            bind_expression(
                expression,
                std::slice::from_ref(&table),
                visible,
                &scope.from_clause(std::slice::from_ref(&table)),
                catalog,
                variables,
            )
        })
        .transpose()?;
    let mut table = table;
    table.alias = Some(alias.to_string());
    table.visible_columns = Some(field_indices.clone());
    table.column_names = renamed_column_names(&table, &field_indices, &output_names, columns, alias, subject, catalog)?;
    Ok((table, merge_predicates(inner_predicate, predicate)))
}

/// Joins two row filters with `AND`, keeping whichever side exists.
fn merge_predicates(left: Option<SqlCatalogBoundExpression>, right: Option<SqlCatalogBoundExpression>) -> Option<SqlCatalogBoundExpression> {
    match (left, right) {
        (None, None) => None,
        (Some(predicate), None) | (None, Some(predicate)) => Some(predicate),
        (Some(left), Some(right)) => {
            Some(SqlCatalogBoundExpression::Binary { left: Box::new(left), operator: SqlBoundBinaryOperator::And, right: Box::new(right) })
        }
    }
}

/// The catalog field indices a flattened body's projection exposes together with
/// the output name of each position.
///
/// `*` exposes every column in catalog order under its catalog name. Otherwise each
/// projection must be a bare column reference (optionally aliased): expressions the
/// body would have to evaluate cannot be flattened into a plain table occurrence, so
/// they return `None`.
fn projected_output_columns(
    projections: &[SqlBoundProjection],
    source: &[SqlCatalogBoundTable],
    catalog: &SqlCatalog,
) -> Result<Option<(Vec<usize>, Vec<String>)>, SqlFrontendError> {
    let table = catalog
        .tables
        .iter()
        .find(|candidate| candidate.type_id == source[0].type_id)
        .ok_or_else(|| internal_error("catalog table identity is not published"))?;
    if matches!(projections, [SqlBoundProjection::Star]) {
        return Ok(Some(((0..table.fields.len()).collect(), table.fields.iter().map(|field| field.name.clone()).collect())));
    }
    let mut indices = Vec::with_capacity(projections.len());
    let mut names = Vec::with_capacity(projections.len());
    for projection in projections {
        let SqlBoundProjection::Expression { expression, alias } = projection
        else {
            return Ok(None);
        };
        let SqlBoundExpression::Identifier(name) = expression
        else {
            return Ok(None);
        };
        let Ok(column) = find_field(name, source, catalog)
        else {
            return Ok(None);
        };
        let index = match table.fields.iter().position(|field| field.field_id == column.field_id) {
            Some(index) => index,
            None => return Ok(None),
        };
        if indices.contains(&index) {
            return Ok(None);
        }
        indices.push(index);
        names.push(alias.clone().unwrap_or_else(|| column.field_name.clone()));
    }
    Ok(Some((indices, names)))
}

/// Applies a flattened body's output names and its explicit CTE column list to the
/// visible column scope.
///
/// The result stays aligned with the catalog field order: catalog names, `field_id`s
/// and the row slot are untouched, only the names this occurrence answers to. Fields
/// the body did not project keep their catalog name behind
/// [`SqlCatalogBoundTable::visible_columns`], which hides them from the outer query.
fn renamed_column_names(
    table: &SqlCatalogBoundTable,
    field_indices: &[usize],
    output_names: &[String],
    columns: &[String],
    alias: &str,
    subject: &str,
    catalog: &SqlCatalog,
) -> Result<Option<Vec<String>>, SqlFrontendError> {
    let catalog_table = catalog
        .tables
        .iter()
        .find(|candidate| candidate.type_id == table.type_id)
        .ok_or_else(|| internal_error("catalog table identity is not published"))?;
    if !columns.is_empty() && columns.len() != field_indices.len() {
        return Err(unsupported_source(
            subject,
            &format!("the explicit column list of `{alias}` names {} columns but its body outputs {}", columns.len(), field_indices.len()),
        ));
    }
    let mut names: Vec<String> =
        catalog_table.fields.iter().enumerate().map(|(index, field)| table.visible_column_name(index, field)).collect();
    for (index, name) in field_indices.iter().zip(output_names) {
        names[*index] = name.clone();
    }
    for (column, index) in columns.iter().zip(field_indices) {
        names[*index] = column.clone();
    }
    Ok(Some(names))
}

fn unsupported_source(subject: &str, reason: &str) -> SqlFrontendError {
    SqlFrontendError::unsupported(format!("SQL {subject} cannot be flattened: {reason}"))
}

fn find_table(catalog: &SqlCatalog, name: &str) -> Result<SqlCatalogBoundTable, SqlFrontendError> {
    let matches = catalog.tables.iter().filter(|table| table.name == name).collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("unknown SQL table: {name}"))),
        [table] => Ok(SqlCatalogBoundTable {
            type_id: table.type_id,
            name: table.name.clone(),
            alias: None,
            row_slot: 0,
            column_names: None,
            visible_columns: None,
        }),
        _ => Err(SqlFrontendError::new(SqlErrorKind::AmbiguousTable, format!("ambiguous SQL table: {name}"))),
    }
}

/// Resolves a column reference against every table visible at the current scope.
///
/// `table.column` spellings resolve inside the named table. Unqualified names must
/// be unique across the visible tables — an ambiguity is reported instead of
/// guessing.
fn find_field(name: &str, sources: &[SqlCatalogBoundTable], catalog: &SqlCatalog) -> Result<SqlCatalogBoundColumn, SqlFrontendError> {
    if sources.is_empty() {
        return Err(SqlFrontendError::new(SqlErrorKind::UnknownColumn, "SQL column reference requires a table source"));
    }
    if let Some((qualifier, column)) = name.split_once('.') {
        let qualifier = qualifier.trim();
        let column = column.trim();
        let source = sources
            .iter()
            .find(|source| source.visible_name() == qualifier)
            .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownTableQualifier, format!("unknown SQL table qualifier: {qualifier}")))?;
        return find_field_in_table(column, source, catalog)?
            .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownColumn, format!("unknown SQL column: {qualifier}.{column}")));
    }
    let mut matches = Vec::new();
    for source in sources {
        if let Some(column) = find_field_in_table(name, source, catalog)? {
            matches.push(column);
        }
    }
    match matches.len() {
        0 => Err(SqlFrontendError::new(SqlErrorKind::UnknownColumn, format!("unknown SQL column: {name}"))),
        1 => Ok(matches.remove(0)),
        _ => Err(SqlFrontendError::new(SqlErrorKind::AmbiguousColumn, format!("ambiguous SQL column: {name} (qualify it with a table name)"))),
    }
}

/// Looks one column up inside a single catalog table.
fn find_field_in_table(
    name: &str,
    source: &SqlCatalogBoundTable,
    catalog: &SqlCatalog,
) -> Result<Option<SqlCatalogBoundColumn>, SqlFrontendError> {
    let table = catalog
        .tables
        .iter()
        .find(|candidate| candidate.type_id == source.type_id)
        .ok_or_else(|| internal_error("catalog table identity is not published"))?;
    // 显式列名把输出列改名后，本 occurrence 的可见作用域整体换名：按基表字段名
    // 解析旧名会读到一个外层看不到的列，因此这里只认这一份可见名。
    let matches = table
        .fields
        .iter()
        .enumerate()
        .filter(|(index, field)| source.exposes_column(*index) && source.visible_column_name(*index, field) == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [(index, field)] => {
            reject_unknown_field(field)?;
            Ok(Some(SqlCatalogBoundColumn {
                type_id: table.type_id,
                field_id: field.field_id,
                row_id: source.row_id(field.field_id),
                table_name: table.name.clone(),
                field_name: field.name.clone(),
                visible_name: source.visible_column_name(*index, field),
                capacity: field.capacity,
            }))
        }
        _ => Err(SqlFrontendError::new(SqlErrorKind::AmbiguousColumn, format!("ambiguous SQL column: {name}"))),
    }
}

/// Returns an `Unsupported` error when the resolved field type has no SQL
/// equivalent (the inner type fell back to [`SqlFieldType::Unknown`]).
///
/// Called from every path that binds a column reference against a catalog —
/// SELECT projections, WHERE clauses, JOIN conditions, UPDATE assignments,
/// INSERT column lists, and `ON CONFLICT DO UPDATE` values. Keeping the check
/// here instead of letting the execution layer crash lets the binder produce
/// one clear diagnostic with the original VOS type name.
pub(crate) fn reject_unknown_field(field: &SqlCatalogField) -> Result<(), SqlFrontendError> {
    if field.field_type.inner() != SqlFieldType::Unknown {
        return Ok(());
    }
    let vos = field.vos_type_name.as_deref().unwrap_or("unknown");
    Err(SqlFrontendError::new(
        SqlErrorKind::Unsupported,
        format!("SQL cannot project field `{}`: VOS type `{vos}` has no SQL equivalent — use VOS operations instead", field.name),
    ))
}

/// Recursively collects the row-value key of every column reference in an expression,
/// preserving first-appearance order and skipping duplicates.
pub fn collect_column_ids(expr: &SqlCatalogBoundExpression, out: &mut Vec<u64>) {
    match expr {
        SqlCatalogBoundExpression::Column(col) => {
            if !out.contains(&col.row_id) {
                out.push(col.row_id);
            }
        }
        SqlCatalogBoundExpression::Function { args, window, filter, .. } => {
            for arg in args {
                collect_column_ids(arg, out);
            }
            if let Some(window) = window {
                for expr in &window.partition_by {
                    collect_column_ids(expr, out);
                }
                for item in &window.order_by {
                    collect_column_ids(&item.expression, out);
                }
            }
            if let Some(predicate) = filter {
                collect_column_ids(predicate, out);
            }
        }
        SqlCatalogBoundExpression::Binary { left, right, .. } => {
            collect_column_ids(left, out);
            collect_column_ids(right, out);
        }
        SqlCatalogBoundExpression::Unary { expression, .. } => collect_column_ids(expression, out),
        SqlCatalogBoundExpression::InSubquery { expression, plan, .. } => {
            collect_column_ids(expression, out);
            crate::sql_planner::collect_plan_outer_row_ids(plan, out);
        }
        SqlCatalogBoundExpression::InValues { expression, .. } => collect_column_ids(expression, out),
        SqlCatalogBoundExpression::Case { branches, else_branch } => {
            for (w, t) in branches {
                collect_column_ids(w, out);
                collect_column_ids(t, out);
            }
            if let Some(els) = else_branch {
                collect_column_ids(els, out);
            }
        }
        SqlCatalogBoundExpression::Cast { expr, .. } => collect_column_ids(expr, out),
        // 子查询对外层列的引用绑成 `OuterColumn`，值取自外层行而不是本层行值，因此
        // 子查询的 `EXISTS` / 标量子查询本身不为本层贡献行值键；外层行的行值键由
        // 外层计划的表达式收集，这里只把子查询计划里的外层引用透传出去。
        SqlCatalogBoundExpression::Exists { plan, .. } | SqlCatalogBoundExpression::ScalarSubquery { plan } => {
            crate::sql_planner::collect_plan_outer_row_ids(plan, out)
        }
        // 外层列不在本层的行值里，不能要求本层行携带它的键。
        SqlCatalogBoundExpression::OuterColumn(_) => {}
        SqlCatalogBoundExpression::Literal(_) | SqlCatalogBoundExpression::Wildcard | SqlCatalogBoundExpression::Interval { .. } => {}
    }
}

fn internal_error(message: &str) -> SqlFrontendError {
    SqlFrontendError::internal(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// uuid 投影成二进制类型，utf16 与 VOS 执行面口径一致地拒绝。
    #[test]
    fn uuid_projects_to_binary_and_utf16_stays_unsupported() {
        assert_eq!(project_resolved_type(&ResolvedCanonicalType::Scalar(CanonicalScalar::Uuid)), SqlFieldType::Uuid);
        assert_eq!(project_resolved_type(&ResolvedCanonicalType::Scalar(CanonicalScalar::Utf8)), SqlFieldType::String);
        assert_eq!(project_resolved_type(&ResolvedCanonicalType::Scalar(CanonicalScalar::Utf16)), SqlFieldType::Unknown);
    }

    /// uuid 文本无论大小写与连字符，都归一到同一组 16 字节与同一段规范文本。
    #[test]
    fn uuid_text_round_trips_through_canonical_octets() {
        let upper = parse_uuid("3F2504E0-4F89-11D3-9A0C-0305E82C3301").expect("hyphenated upper case");
        let plain = parse_uuid("3f2504e04f8911d39a0c0305e82c3301").expect("32 hex digits");
        assert_eq!(upper, plain);
        assert_eq!(upper.len(), 16);
        assert_eq!(uuid_text(&upper).as_deref(), Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301"));
        assert!(parse_uuid("not-a-uuid").is_none());
    }

    /// uuid 列交付 16 字节规范值，文本列保留原始文本值。
    #[test]
    fn storage_value_of_uuid_is_sixteen_octets() {
        let literal = SqlBoundLiteral::String("3F2504E0-4F89-11D3-9A0C-0305E82C3301".to_string());
        let uuid = SqlFieldType::Uuid.storage_value_of(&literal).unwrap().expect("uuid value");
        assert!(matches!(uuid, database_types::Value::Uuid(_)));
        let bytes = database_formats::yy::row::encode_tagged(&uuid).expect("encode tagged uuid");
        assert_eq!(bytes.len(), 17, "tag + 16 octets");
        assert!(matches!(SqlFieldType::String.storage_value_of(&literal).unwrap(), Some(database_types::Value::Text(_))));
        assert!(SqlFieldType::Uuid.storage_value_of(&SqlBoundLiteral::Null).unwrap().is_none());
        assert!(SqlFieldType::Uuid.storage_value_of(&SqlBoundLiteral::String("nope".to_string())).is_err());
    }

    #[test]
    fn json_literals_become_structured_runtime_values() {
        let value = SqlFieldType::Json
            .storage_value_of(&SqlBoundLiteral::String(r#"{"a":[1,true,null]}"#.to_string()))
            .unwrap()
            .expect("structured json");
        assert!(matches!(value, database_types::Value::Row(_)));
    }

    #[test]
    fn integer_literals_are_range_checked_against_the_column_width() {
        assert!(SqlFieldType::Int16.storage_value_of(&SqlBoundLiteral::Integer(70_000)).is_err());
        assert_eq!(
            SqlFieldType::Int32.storage_value_of(&SqlBoundLiteral::Integer(70_000)).unwrap(),
            Some(database_types::Value::I64(70_000))
        );
    }
}
