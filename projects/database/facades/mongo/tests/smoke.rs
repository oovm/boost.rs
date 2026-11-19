//! 最小 smoke：`.yykv` 后端写入、扫描并重开。
//!
//! 详细合同与命令语义矩阵在下游 `mongo-conformance`，上游只保留这条往返。

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use database_protocol_mongo::bson::{BsonDocument, BsonValue, encode_bson, value_to_key};
use mongo::{MongoStorage, Store};

fn scratch() -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_nanos()).unwrap_or(0);
    std::env::temp_dir().join(format!("mongo-smoke-{}-{stamp}.yykv", std::process::id()))
}

#[test]
fn store_round_trips_one_document() {
    let base = scratch();
    let store = Store::open(&base).expect("store opens");
    let storage: &dyn MongoStorage = &store;

    let mut document = BsonDocument::new();
    document.push("_id".to_string(), BsonValue::Int32(7));
    let entry = (value_to_key(&BsonValue::Int32(7)), encode_bson(&document));
    storage.insert("smoke.items", &[entry], true).expect("insert lands");
    assert_eq!(storage.scan("smoke.items").expect("scan lands").len(), 1);

    drop(store);
    let reopened = Store::open(&base).expect("store reopens");
    assert!(reopened.namespace_exists("smoke.items").expect("catalog is readable"));

    let _ = std::fs::remove_file(base.with_extension("yyds"));
    let _ = std::fs::remove_file(base.with_extension("yykv"));
}
