//! 规划纯度：placement 只在执行阶段经正式存储端口落对象。
//!
//! 规划（含重复规划）只产出纯的放置意图：行值是内容寻址引用，由载荷哈希
//! 推导，规划路径不写对象存储；执行路径经 `ObjectStore` 提交同一引用。
//! 这钉死 §12.4.2 的合同：规划失败、重复规划不得写对象或分配持久 id。

use std::collections::BTreeMap;

use database_execution::engine::{
    Connection,
    bytes::{Placement, object_reference},
};
use database_formats::yy::yydx::blob_chunk_hash;
use database_query::local::{point_write_keys, resolved_contract_from_document};
use database_types::{INLINE_BYTES_MAX, ObjectKind, Value};

const SCHEMA: &str = "table Blob { @@id: i64, body: bytes }";

fn connection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    conn
}

fn large_payload() -> Vec<u8> {
    vec![0xA5u8; INLINE_BYTES_MAX + 1]
}

fn small_payload() -> Vec<u8> {
    vec![0x5Au8; 16]
}

/// 大载荷在 CAS 中的引用：与执行侧 `put_chunk` 同一推导，纯函数。
fn expected_reference(payload: &[u8]) -> database_types::ObjectRef {
    object_reference(ObjectKind::Blob, payload)
}

fn parse_program(source: &str) -> schema_input_vos::syntax::VosProgram {
    schema_input_vos::parse_program(source).expect("fixture parses")
}

#[test]
fn planning_derives_the_committed_reference_without_writing() {
    let conn = connection();
    let payload = large_payload();
    let source = r#"Blob.insert({ id: 1, body: __yydb_param_0 })"#;
    let program = parse_program(source);
    let contract = resolved_contract_from_document(SCHEMA).unwrap();
    let object = expected_reference(&payload);

    // 规划两次：键一致，且对象始终没有被写入。
    for _ in 0..2 {
        let keys = point_write_keys(
            &program,
            &contract,
            &[],
            None,
            &[Value::Bytes(payload.clone())],
            true,
            &mut |value| Placement::Preview.place(value),
        )
        .unwrap();
        assert!(keys.is_some(), "a keyed insert plans to a point write");
        let error = conn.get_object(&object).expect_err("planning must not write the object");
        assert!(format!("{error:?}").contains("not found") || format!("{error:?}").contains("ObjectNotFound"), "unexpected: {error:?}");
    }

    // 执行同一语句：对象经正式存储端口落一次，行读回原载荷。
    let mut named = BTreeMap::new();
    named.insert("body".to_owned(), Value::Bytes(payload.clone()));
    conn.execute_vos_write_with_params(r#"Blob.insert({ id: 1, body: $body })"#, &named).unwrap();
    let stored = conn.get_object(&object).expect("execution writes the payload");
    assert_eq!(stored.as_ref(), payload.as_slice());
    let rows = conn.query_vos("Blob.all().collect()").unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0]["body"] {
        Value::Bytes(octets) => assert_eq!(octets.as_slice(), payload.as_slice()),
        other => panic!("the reference must resolve back to the payload, got {other:?}"),
    }
}

#[test]
fn failed_planning_leaves_no_object_behind() {
    let conn = connection();
    let payload = large_payload();
    // 未知字段：规划期就报错，载荷绝不落对象存储。
    let source = r#"Blob.insert({ id: 1, ghost: __yydb_param_0 })"#;
    let program = parse_program(source);
    let contract = resolved_contract_from_document(SCHEMA).unwrap();
    let keys = point_write_keys(
        &program,
        &contract,
        &[],
        None,
        &[Value::Bytes(payload.clone())],
        true,
        &mut |value| Placement::Preview.place(value),
    );
    assert!(keys.is_err() || keys.unwrap().is_none(), "an unknown field cannot plan as a point write");
    let object = expected_reference(&payload);
    assert!(conn.get_object(&object).is_err(), "a failed plan leaves no object");

    // 完整入口同样失败且无副作用。
    let mut named = BTreeMap::new();
    named.insert("ghost".to_owned(), Value::Bytes(payload.clone()));
    let error = conn.execute_vos_write_with_params(r#"Blob.insert({ id: 1, ghost: $ghost })"#, &named);
    assert!(error.is_err(), "the write must fail");
    assert!(conn.get_object(&object).is_err(), "the failed write left no object");
    assert!(conn.query_vos("Blob.all().collect()").unwrap().is_empty(), "and no row");
}

#[test]
fn small_payloads_stay_inline_and_never_touch_the_store() {
    let conn = connection();
    let payload = small_payload();
    let mut named = BTreeMap::new();
    named.insert("body".to_owned(), Value::Bytes(payload.clone()));
    conn.execute_vos_write_with_params(r#"Blob.insert({ id: 1, body: $body })"#, &named).unwrap();
    // 小载荷留在行内：CAS 中没有它的条目。
    let object = expected_reference(&payload);
    assert!(conn.get_object(&object).is_err(), "inline payloads are never stored as objects");
    let rows = conn.query_vos("Blob.all().collect()").unwrap();
    assert!(matches!(&rows[0]["body"], Value::Bytes(octets) if octets.as_slice() == payload.as_slice()));
}

#[test]
fn preview_and_commit_agree_on_the_row_value() {
    let payload = large_payload();
    // 预览得到的引用与直接按载荷推导的引用逐字节一致（哈希合同）。
    let previewed = Placement::Preview.place(Value::Bytes(payload.clone())).unwrap();
    let hash = blob_chunk_hash(&payload);
    match previewed {
        Value::Object(object) => {
            assert_eq!(object.hash, hash);
            assert_eq!(object.size, payload.len() as u64);
            assert_eq!(object.kind, ObjectKind::Blob);
        }
        other => panic!("a large payload must preview to an object reference, got {other:?}"),
    }
}
