//! 多文档 schema：跨命名空间引用解析进同一份契约。

use database_execution::engine::Connection;
use database_execution::program::Type;

const CORE: &str = "\
namespace oai_autos

table User {
    @@id: uuid,
    name: utf8,
}
";

const LOGS: &str = "\
namespace oai_autos_logs

table Entry {
    @@id: uuid,
    user_id: &oai_autos::User,
    note: utf8?,
}
";

#[test]
fn cross_namespace_references_resolve_into_one_contract() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema_files(&[CORE, LOGS]).expect("multi-document schema must resolve");

    let catalog = conn.execution_catalog().unwrap().expect("catalog");
    let entry = catalog.types.iter().find(|item| item.name == "Entry").expect("Entry table");
    let user_id = entry.fields.iter().find(|field| field.name == "user_id").expect("user_id field");
    assert_eq!(user_id.handle.ty(), Type::Uuid, "`&oai_autos::User` must lower to the target primary key");

    // 同一组文档重复装配是幂等的：持久化的是拼接后的同一份文本。
    conn.ensure_schema_files(&[CORE, LOGS]).expect("re-installing the same documents must be a no-op");
}
