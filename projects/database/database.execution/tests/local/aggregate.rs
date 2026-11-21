//! Aggregates over exact scalar columns.
//!
//! `SUM(DECIMAL)` must stay in the exact decimal family: MySQL returns a
//! `DECIMAL`, never `NULL`, for a group of decimal rows.

use std::collections::BTreeMap;

use database_execution::local::execute_sql;
use database_query::local::resolved_contract_from_document;
use database_types::Value;

const SCHEMA: &str = r#"
table Ledger {
    @@id: i64,
    amount: Decimal<12, 2>,
}
"#;

fn ledger() -> (schema_types::ResolvedContract, BTreeMap<String, Vec<u8>>) {
    let contract = resolved_contract_from_document(SCHEMA).expect("ledger schema resolves");
    let mut records = BTreeMap::new();
    execute_sql(
        "INSERT INTO Ledger (id, amount) VALUES (1, '1.50'), (2, '10.00'), (3, '-2.25')",
        &[],
        &contract,
        &[],
        &mut records,
    )
    .expect("seed ledger");
    (contract, records)
}

#[test]
fn sum_over_decimal_keeps_the_exact_total() {
    let (contract, mut records) = ledger();

    // `1.50 + 10.00 - 2.25 = 9.25`. The exact decimal family must survive the
    // aggregate instead of being skipped (which would report NULL).
    let rows = execute_sql("SELECT SUM(amount) AS total FROM Ledger", &[], &contract, &[], &mut records).expect("decimal sum").rows;
    assert_eq!(rows.len(), 1);
    match rows[0].get("total") {
        Some(Value::Decimal(total)) => {
            assert_eq!(total.unscaled(), 925, "SUM must keep the exact unscaled total");
            assert_eq!(total.scale(), 2, "SUM keeps the column scale");
        }
        other => panic!("SUM(DECIMAL) must stay exact, got {other:?}"),
    }
}
