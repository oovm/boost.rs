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

table Payment {
    @@payment_id: i64,
    account_id: i64,
    amount: i64,
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
    for (payment_id, account_id, amount) in [(10, 1, 5), (11, 1, 7), (12, 2, 3)] {
        execute_sql(
            &format!("INSERT INTO Payment (payment_id, account_id, amount) VALUES ({payment_id}, {account_id}, {amount})"),
            &[],
            &contract,
            &[],
            &mut records,
        )
        .expect("seed payment");
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

fn run(sql: &str, contract: &schema_types::ResolvedContract, records: &BTreeMap<String, Vec<u8>>) -> Vec<BTreeMap<String, Value>> {
    let stmt = prepare(sql).unwrap_or_else(|error| panic!("prepare `{sql}`: {error}"));
    execute_prepared(&stmt, &[], Some(contract), records).unwrap_or_else(|error| panic!("execute `{sql}`: {error}"))
}

#[test]
fn star_projection_keeps_every_public_field() {
    let (contract, records) = seeded();
    let rows = run("Account.all().map(a => a.*).collect()", &contract, &records);
    assert_eq!(rows.len(), 3);
    let ada = &rows[0];
    assert!(ada.contains_key("account_id") && ada.contains_key("email") && ada.contains_key("score"), "x.* keeps the full public shape");
}

#[test]
fn computed_projection_arithmetic_and_functions() {
    let (contract, records) = seeded();
    let rows = run("Account.all().map(a => a.{ doubled: a.score * 2, who: a.email.upper() }).collect()", &contract, &records);
    let doubled: Vec<_> = rows.iter().map(|row| match row.get("doubled") {
        Some(Value::I64(n)) => *n,
        other => panic!("unexpected doubled {other:?}"),
    }).collect();
    assert_eq!(doubled, vec![2, 4, 6]);
    let who: Vec<_> = rows.iter().map(|row| match row.get("who") {
        Some(Value::Text(text)) => text.clone(),
        other => panic!("unexpected who {other:?}"),
    }).collect();
    assert_eq!(who, vec!["ADA@EXAMPLE.TEST", "LINUS@EXAMPLE.TEST", "GRACE@OTHER.TEST"]);
}

#[test]
fn computed_predicates_and_negation() {
    let (contract, records) = seeded();
    assert_eq!(
        emails(r#"Account.all().filter(a => a.score * 2 > 2).collect()"#, &contract, &records),
        vec!["Linus@Example.Test", "grace@other.test"]
    );
    assert_eq!(
        emails(r#"Account.all().filter(a => !a.email.contains("ada")).collect()"#, &contract, &records).len(),
        2,
        "`!` negates the whole predicate"
    );
    assert_eq!(
        emails(r#"Account.all().filter(a => a.score > 1.5).collect()"#, &contract, &records).len(),
        2,
        "float literals are usable in field comparisons now"
    );
    assert_eq!(
        emails(r#"Account.all().filter(a => a.account_id >= a.score).collect()"#, &contract, &records).len(),
        3,
        "field-to-field comparisons lower to the shared two-sided predicate"
    );
}

#[test]
fn sort_by_supports_directions_and_multiple_keys() {
    let (contract, records) = seeded();
    assert_eq!(
        emails(r#"Account.all().sort_by(a => a.score.desc()).collect()"#, &contract, &records),
        vec!["grace@other.test", "Linus@Example.Test", "ada@example.test"]
    );
    assert_eq!(
        emails(r#"Account.all().sort_by(a => a.account_id.asc(), a => a.email.desc()).collect()"#, &contract, &records),
        vec!["ada@example.test", "Linus@Example.Test", "grace@other.test"],
        "the first key decides; the suffix overrides the method default"
    );
    assert_eq!(
        emails(r#"Account.all().sort_by_desc(a => a.score).collect()"#, &contract, &records),
        vec!["grace@other.test", "Linus@Example.Test", "ada@example.test"]
    );
}

#[test]
fn execution_boundaries_return_their_locked_shapes() {
    let (contract, records) = seeded();
    let count = |sql: &str| match &run(sql, &contract, &records)[..] {
        [row] => match row.get("count") {
            Some(Value::U64(n)) => *n,
            Some(Value::I64(n)) => *n as u64,
            other => panic!("unexpected count {other:?}"),
        },
        rows => panic!("count must yield exactly one row, got {}", rows.len()),
    };
    assert_eq!(count("Account.count()"), 3);
    assert_eq!(count("Account.count(a => a.score > 1)"), 2);

    let rows = run("Account.get(2)", &contract, &records);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].get("email"), Some(Value::Text(email)) if email == "Linus@Example.Test"));
    assert!(run("Account.get(99)", &contract, &records).is_empty(), "a missing primary key is zero rows");

    let first = run("Account.first(a => a.score > 1)", &contract, &records);
    assert_eq!(first.len(), 1);
    assert!(matches!(first[0].get("email"), Some(Value::Text(_))));

    let yes = run(r#"Account.any(a => a.email == "ada@example.test")"#, &contract, &records);
    assert!(matches!(yes[0].get("any"), Some(Value::Bool(true))));
    let no = run(r#"Account.any(a => a.email == "nobody")"#, &contract, &records);
    assert!(matches!(no[0].get("any"), Some(Value::Bool(false))));
}

#[test]
fn unsupported_surface_is_rejected_not_guessed() {
    let (contract, records) = seeded();
    assert!(prepare("Account.all().map(a => a.{ x: a.* }).collect()").is_err(), "`*` inside an explicit projection is not v1");
    assert!(prepare("Account.all().limit(1).collect()").is_err(), "undocumented methods must fail closed");
    assert!(prepare("Account.count().collect()").is_err(), "a boundary is not a collect pipeline");
    let _ = (&contract, &records);
}

fn single(rows: Vec<BTreeMap<String, Value>>, column: &str) -> Value {
    match &rows[..] {
        [row] => row.get(column).cloned().unwrap_or(Value::Null),
        other => panic!("expected exactly one row, got {}", other.len()),
    }
}

#[test]
fn whole_table_aggregates_match_their_sql_counterparts() {
    let (contract, records) = seeded();
    assert!(matches!(single(run("Payment.sum(p => p.amount)", &contract, &records), "sum"), Value::I64(15)));
    assert!(
        matches!(single(run("Payment.avg(p => p.amount)", &contract, &records), "avg"), Value::F64(avg) if (avg - 5.0).abs() < 1e-9),
        "AVG always yields a finite float"
    );
    assert!(matches!(single(run("Payment.min(p => p.amount)", &contract, &records), "min"), Value::I64(3)));
    assert!(matches!(single(run("Payment.max(p => p.amount)", &contract, &records), "max"), Value::I64(7)));
    // The same aggregates close a pipeline rather than a bare table.
    assert!(matches!(
        single(run("Payment.all().filter(p => p.account_id == 1).sum(p => p.amount)", &contract, &records), "sum"),
        Value::I64(12)
    ));
}

#[test]
fn aggregate_filters_match_sql_filter_where() {
    let (contract, records) = seeded();
    // `FILTER (WHERE ...)` trims the rows that reach the aggregate.
    assert!(matches!(single(run("Payment.sum(p => p.amount, p => p.account_id == 1)", &contract, &records), "sum"), Value::I64(12)));
    assert!(matches!(single(run("Payment.count(p => p.account_id == 2)", &contract, &records), "count"), Value::I64(1)));
    assert!(matches!(single(run("Payment.all().count()", &contract, &records), "count"), Value::I64(3)));
    // A projection made only of aggregates folds every row into one.
    let rows = run(
        "Payment.all().map(p => p.{ n: count(), total: p.amount.sum(), big: p.amount.sum(q => q.amount > 4) }).collect()",
        &contract,
        &records,
    );
    assert!(matches!(single(rows.clone(), "n"), Value::I64(3)));
    assert!(matches!(single(rows.clone(), "total"), Value::I64(15)));
    assert!(matches!(single(rows, "big"), Value::I64(12)), "the filter keeps only the two payments above four");
}

#[test]
fn grouped_aggregates_match_sql_group_by() {
    let (contract, records) = seeded();
    let rows = run(
        "Payment.all().group_by(p => p.account_id).map(p => p.{ account_id, total: p.amount.sum(), n: count() }).sort_by(p => p.account_id).collect()",
        &contract,
        &records,
    );
    assert_eq!(rows.len(), 2, "one row per distinct account");
    assert!(matches!(rows[0].get("account_id"), Some(Value::I64(1))));
    assert!(matches!(rows[0].get("total"), Some(Value::I64(12))));
    assert!(matches!(rows[0].get("n"), Some(Value::I64(2))));
    assert!(matches!(rows[1].get("account_id"), Some(Value::I64(2))));
    assert!(matches!(rows[1].get("total"), Some(Value::I64(3))));
    assert!(matches!(rows[1].get("n"), Some(Value::I64(1))));
}

#[test]
fn array_agg_keeps_group_order() {
    let (contract, records) = seeded();
    let rows = run("Payment.all().sort_by(p => p.amount).array_agg(p => p.amount)", &contract, &records);
    assert!(matches!(
        single(rows, "array_agg"),
        Value::List(values) if values == vec![Value::I64(3), Value::I64(5), Value::I64(7)]
    ));
}

#[test]
fn join_addresses_both_sides_by_their_lambda_names() {
    let (contract, records) = seeded();
    let rows = run(
        "Account.join(Payment.all(), (a, pay) => a.account_id == pay.account_id).map(x => x.{ id: x.a.account_id, email: x.a.email, amount: x.pay.amount }).sort_by(x => x.amount).collect()",
        &contract,
        &records,
    );
    assert_eq!(rows.len(), 3);
    let amounts: Vec<i64> = rows
        .iter()
        .map(|row| match row.get("amount") {
            Some(Value::I64(amount)) => *amount,
            other => panic!("unexpected amount {other:?}"),
        })
        .collect();
    assert_eq!(amounts, vec![3, 5, 7]);
    assert!(matches!(rows[0].get("email"), Some(Value::Text(email)) if email == "Linus@Example.Test"));
}

#[test]
fn left_join_pads_unmatched_rows_with_null() {
    let (contract, records) = seeded();
    let rows = run(
        "Account.left_join(Payment.all(), (a, pay) => a.account_id == pay.account_id).map(x => x.{ id: x.a.account_id, amount: x.pay.amount }).sort_by(x => x.id).collect()",
        &contract,
        &records,
    );
    assert_eq!(rows.len(), 4, "grace has no payment and still appears");
    assert!(matches!(rows[3].get("amount"), Some(Value::Null)));
}

#[test]
fn grouped_and_join_surface_fails_closed() {
    // A grouped projection may only name group keys and aggregates.
    assert!(
        prepare("Payment.all().group_by(p => p.account_id).map(p => p.{ amount: p.amount }).collect()").is_err(),
        "a non-aggregate field must be a group key"
    );
    // Nothing may run between `.group_by` and the projection that consumes it.
    assert!(
        prepare("Payment.all().group_by(p => p.account_id).filter(p => p.amount > 0).map(p => p.{ account_id }).collect()").is_err(),
        "a filter after group_by would silently filter input rows"
    );
    // Joins must bind both sides so every path is unambiguous.
    assert!(prepare("Account.join(Payment.all(), a => a.account_id == 1).map(x => x.*).collect()").is_err());
    // Aggregates still take at most one filter predicate.
    assert!(prepare("Payment.sum(p => p.amount, p => p.amount > 0, p => p.amount > 1)").is_err());
}
