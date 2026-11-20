//! VOS frontend: text operators lowered into the shared predicate model.

use std::collections::BTreeMap;

use database_execution::local::{execute_prepared, execute_sql};
use database_query::local::{prepare, resolved_contract_from_document};
use database_types::Value;

const SCHEMA: &str = r#"
table Account {
    @@account_id: i64,
    email: utf8,
    score: i64,
}
"#;

fn seeded() -> (schema_types::ResolvedContract, BTreeMap<String, Vec<u8>>) {
    let contract = resolved_contract_from_document(SCHEMA).expect("account schema resolves");
    let mut records = BTreeMap::new();
    for (id, email) in [(1, "ada@example.test"), (2, "Linus@Example.Test"), (3, "grace@other.test")] {
        execute_sql(
            &format!("INSERT INTO Account (account_id, email, score) VALUES ({id}, '{email}', {id})"),
            &[],
            &contract,
            &[],
            &mut records,
        )
        .expect("seed row");
    }
    (contract, records)
}

fn emails(sql: &str, contract: &schema_types::ResolvedContract, records: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let stmt = prepare(sql).expect("prepare VOS pipeline");
    let rows = execute_prepared(&stmt, &[], Some(contract), records).expect("execute VOS pipeline");
    rows.iter()
        .map(|row| match row.get("email") {
            Some(Value::Text(email)) => email.clone(),
            other => panic!("unexpected email {other:?}"),
        })
        .collect()
}

#[test]
fn like_matches_sql_wildcards() {
    let (contract, records) = seeded();
    assert_eq!(emails(r#"Account.all().filter(a => a.email.like("ada%")).collect()"#, &contract, &records), vec!["ada@example.test"]);
    assert_eq!(
        emails(r#"Account.all().filter(a => a.email.like("%@example.test")).collect()"#, &contract, &records),
        vec!["ada@example.test"],
        "% must match an arbitrary run, and LIKE stays case-sensitive"
    );
    assert_eq!(
        emails(r#"Account.all().filter(a => a.email.ilike("%@example.test")).collect()"#, &contract, &records).len(),
        2,
        "the case-insensitive fold is what makes the second mixed-case address match"
    );
    assert_eq!(
        emails(r#"Account.all().filter(a => a.email.like("_da%")).collect()"#, &contract, &records),
        vec!["ada@example.test"],
        "_ must match exactly one code point"
    );
    assert!(emails(r#"Account.all().filter(a => a.email.like("ADA%")).collect()"#, &contract, &records).is_empty(), "LIKE is case-sensitive");
}

#[test]
fn ilike_folds_case_like_the_sql_frontend() {
    let (contract, records) = seeded();
    assert_eq!(emails(r#"Account.all().filter(a => a.email.ilike("ADA%")).collect()"#, &contract, &records), vec!["ada@example.test"]);
    assert_eq!(
        emails(r#"Account.all().filter(a => a.email.ilike("linus%@example.test")).collect()"#, &contract, &records),
        vec!["Linus@Example.Test"]
    );
}

#[test]
fn like_accepts_a_bind_parameter() {
    let (contract, records) = seeded();
    // A pattern can be a bind parameter, like every other text test.
    let stmt = prepare(r#"Account.all().filter(a => a.email.like($pattern)).collect()"#).expect("prepare parameterised like");
    let bound = database_query::local::bind_named(&stmt, &BTreeMap::from([("pattern".to_owned(), Value::Text("g%".to_owned()))]))
        .expect("bind pattern");
    let rows = execute_prepared(&stmt, &bound, Some(&contract), &records).expect("execute parameterised like");
    assert_eq!(rows.len(), 1);
}
