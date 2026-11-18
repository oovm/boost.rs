//! `yydb` 门面：嵌入式 YYDB 的产品作者面。
//!
//! 下游绑定（N-API / PyO3 / WASM）只持有本门面的资源句柄并做宿主类型映射,
//! 不在绑定内实现数据库语义。能力按 `database` 门面的同一命名再导出,
//! 远程客户端经 [`client`]（即 `yydb-client`）提供。

#![deny(missing_docs)]

/// 远程客户端门面。
pub mod client {
    pub use yydb_client::*;
}

pub use database::{cluster, embed, execution, formats, harness, input, migration, protocol, query, server, storage, types, udf};

/// 文件对象与 schema 合同（对象 API 的描述符与读取窗口类型）。
pub mod schema {
    pub use schema_types::*;
}
