//! SQL 值投影：行记录里的运行值按声明列类型还原成 SQL 值。
//!
//! 这里钉住精确小数（`Value::Decimal`）的投影：VOS 侧按 `Decimal<p, s>` 落库的字段
//! 必须是它的精确十进制文本，而不是类型名；否则 `SUM`/比较一类数值消费者会把整列
//! 当成非数值而返回 NULL。

use database_formats::yy::row::encode_tagged;
use database_query::{RowFieldValues, SqlValue, catalog_bind::SqlFieldType};
use database_types::{Decimal, Value};

const AMOUNT: u64 = 7;

fn row_with(value: &Value) -> RowFieldValues {
    let mut row = RowFieldValues::new();
    row.insert(AMOUNT, encode_tagged(value).expect("tagged value encodes"));
    row
}

#[test]
fn decimal_cell_projects_its_exact_text() {
    let decimal = Decimal::new(123_456, 8, 3).expect("decimal shape is valid");
    let row = row_with(&Value::Decimal(decimal));

    assert_eq!(
        row.value_of(AMOUNT, &SqlFieldType::Decimal),
        Some(SqlValue::String("123.456".into())),
        "an exact decimal is numeric text, never the type name"
    );
    // 没有列类型信息时也按自然投影保留精确文本；SUM 这类消费者据此解析数值。
    assert_eq!(
        row.value_of(AMOUNT, &SqlFieldType::Unknown),
        Some(SqlValue::String("123.456".into())),
        "an unknown-typed decimal cell keeps its exact text"
    );
}
