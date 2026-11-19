//! `yyds-sql-schema/v0` 目录文档编解码（`sql-schema.v0.json`）。
//!
//! SQL 网关把本地 schema 目录持久为一份严格 JSON 文档，重启后读回同一目录。
//! 本模块只拥有该文档的字节形状：字段名、合同标签、未知字段拒绝与版本校验。
//! 内存模型与文档之间的转换、加载后的语义校验与文件发布都不在这里。

use serde::{Deserialize, Serialize};

/// 每份持久化 schema 文档写入的合同标签。
pub const CONTRACT_TAG: &str = "yyds-sql-schema/v0";

/// 编解码该文档形状时的失败，与宿主错误类型无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSchemaCodecError {
    /// 文档不是当前合同：未知合同标签或无法解析的 JSON。
    Contract(String),
    /// JSON 可解析但形状不合合同（缺字段、多余字段、类型不符）。
    Invalid(String),
}

impl std::fmt::Display for SqlSchemaCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(reason) => write!(formatter, "unsupported SQL schema document: {reason}"),
            Self::Invalid(reason) => write!(formatter, "invalid SQL schema document: {reason}"),
        }
    }
}

impl std::error::Error for SqlSchemaCodecError {}

/// 一份完整的持久化 schema 目录文档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSchemaDocument {
    /// 合同标签，必须等于 [`CONTRACT_TAG`]。
    pub contract: String,
    /// 目录修订号，非零。
    pub revision: u32,
    /// 下一个可分配的表类型 id。
    pub next_type_id: u64,
    /// 下一个可分配的列 id。
    pub next_field_id: u64,
    /// 已登记的表。
    pub tables: Vec<SqlSchemaTableDocument>,
    /// 已登记的索引。
    pub indexes: Vec<SqlSchemaIndexDocument>,
}

/// 文档中的一张表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSchemaTableDocument {
    /// 表名。
    pub name: String,
    /// 稳定的表类型 id。
    pub type_id: u64,
    /// 主键列名。
    pub primary_key: Vec<String>,
    /// 表级唯一约束的列名组。
    pub unique_constraints: Vec<Vec<String>>,
    /// 表级 CHECK 表达式文本。
    pub check_constraints: Vec<String>,
    /// 外键约束。
    pub foreign_keys: Vec<SqlSchemaForeignKeyDocument>,
    /// 列。
    pub fields: Vec<SqlSchemaFieldDocument>,
}

/// 文档中的一条外键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSchemaForeignKeyDocument {
    /// 子表列名。
    pub columns: Vec<String>,
    /// 父表名。
    pub references_table: String,
    /// 父表列名。
    pub references_columns: Vec<String>,
    /// `ON DELETE` 动作文本。
    pub on_delete: Option<String>,
    /// `ON UPDATE` 动作文本。
    pub on_update: Option<String>,
}

/// 文档中的一列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSchemaFieldDocument {
    /// 列名。
    pub name: String,
    /// 稳定的列 id。
    pub field_id: u64,
    /// 基础字段类型。
    #[serde(rename = "type")]
    pub field_type: SqlSchemaFieldTypeDocument,
    /// 是否可空。
    pub nullable: bool,
    /// 声明的字符/字节容量。
    pub capacity: Option<u32>,
    /// 书写的 SQL 类型文本。
    pub sql_type: String,
    /// `DEFAULT` 表达式文本。
    #[serde(default)]
    pub default: Option<String>,
    /// 是否持有值序列。
    #[serde(default)]
    pub auto_increment: bool,
    /// 序列下一个发放值。
    #[serde(default = "initial_next_value")]
    pub next_value: u64,
    /// 列内 `CHECK` 表达式文本。
    pub check: Option<String>,
    /// 生成列表达式文本。
    #[serde(default)]
    pub generated: Option<String>,
}

/// 文档中的一个索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSchemaIndexDocument {
    /// 索引名。
    pub name: String,
    /// 所属表名。
    pub table: String,
    /// 是否唯一索引。
    pub unique: bool,
    /// 索引列名。
    pub columns: Vec<String>,
    /// 部分索引 `WHERE` 谓词文本。
    pub predicate: Option<String>,
}

/// 文档中的字段类型拼写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlSchemaFieldTypeDocument {
    /// 32 位整数。
    Int32,
    /// 16 位整数（PostgreSQL `smallint`）。
    Int16,
    /// 64 位整数。
    Int64,
    /// 64 位浮点。
    Float64,
    /// 文本。
    String,
    /// 布尔。
    Bool,
    /// 字节串。
    Bytes,
    /// 定点小数。
    Decimal,
    /// 日期。
    Date,
    /// 日期时间。
    Datetime,
    /// UUID。
    Uuid,
    /// JSON / JSONB 文本载荷。
    Json,
    /// 未知类型。
    Unknown,
}

/// 新序列的首个发放值。
fn initial_next_value() -> u64 {
    1
}

/// 把文档编码为带缩进的 JSON 文本。
pub fn encode(document: &SqlSchemaDocument) -> Result<String, SqlSchemaCodecError> {
    serde_json::to_string_pretty(document).map_err(|error| SqlSchemaCodecError::Invalid(error.to_string()))
}

/// 从 JSON 文本解码文档，先拒绝未知合同再校验形状。
pub fn decode(source: &str) -> Result<SqlSchemaDocument, SqlSchemaCodecError> {
    let document: SqlSchemaDocument = serde_json::from_str(source).map_err(|error| SqlSchemaCodecError::Contract(error.to_string()))?;
    if document.contract != CONTRACT_TAG {
        return Err(SqlSchemaCodecError::Contract(format!(
            "unsupported SQL schema contract `{}`, expected `{CONTRACT_TAG}`",
            document.contract
        )));
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SqlSchemaDocument {
        SqlSchemaDocument {
            contract: CONTRACT_TAG.to_string(),
            revision: 3,
            next_type_id: 5,
            next_field_id: 9,
            tables: vec![SqlSchemaTableDocument {
                name: "users".into(),
                type_id: 4,
                primary_key: vec!["id".into()],
                unique_constraints: vec![vec!["email".into()]],
                check_constraints: vec!["quota >= 0".into()],
                foreign_keys: vec![SqlSchemaForeignKeyDocument {
                    columns: vec!["tenant_id".into()],
                    references_table: "tenants".into(),
                    references_columns: vec!["id".into()],
                    on_delete: Some("CASCADE".into()),
                    on_update: None,
                }],
                fields: vec![
                    SqlSchemaFieldDocument {
                        name: "id".into(),
                        field_id: 7,
                        field_type: SqlSchemaFieldTypeDocument::Int64,
                        nullable: false,
                        capacity: None,
                        sql_type: "BIGSERIAL".into(),
                        default: None,
                        auto_increment: true,
                        next_value: 12,
                        check: None,
                        generated: None,
                    },
                    SqlSchemaFieldDocument {
                        name: "email".into(),
                        field_id: 8,
                        field_type: SqlSchemaFieldTypeDocument::String,
                        nullable: true,
                        capacity: Some(320),
                        sql_type: "VARCHAR(320)".into(),
                        default: None,
                        auto_increment: false,
                        next_value: 1,
                        check: Some("email <> ''".into()),
                        generated: None,
                    },
                ],
            }],
            indexes: vec![SqlSchemaIndexDocument {
                name: "idx_email".into(),
                table: "users".into(),
                unique: true,
                columns: vec!["email".into()],
                predicate: Some("email IS NOT NULL".into()),
            }],
        }
    }

    #[test]
    fn document_round_trips_through_the_contract_text() {
        let document = sample();
        let encoded = encode(&document).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded, document);
        assert!(encoded.contains("\"contract\": \"yyds-sql-schema/v0\""));
    }

    #[test]
    fn unknown_contract_is_rejected() {
        let mut document = sample();
        document.contract = "yyds-sql-schema/v9".into();
        let encoded = encode(&document).expect("encode");
        let error = decode(&encoded).expect_err("unknown contract must fail");
        assert!(matches!(error, SqlSchemaCodecError::Contract(_)));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let encoded = encode(&sample()).expect("encode");
        let injected = encoded.replace("\"revision\": 3", "\"revision\": 3, \"surprise\": 1");
        let error = decode(&injected).expect_err("unknown field must fail");
        assert!(matches!(error, SqlSchemaCodecError::Contract(_)));
    }
}
