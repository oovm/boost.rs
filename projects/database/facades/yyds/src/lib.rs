//! `yyds` 门面：分片节点的资源组装、线服务装配与远程客户端。
//!
//! 下游绑定（N-API / PyO3 / WASM）只应持有本门面的资源句柄并做宿主类型映射,
//! 不在绑定内实现执行器装配、线服务循环或协议分派。
//!
//! - [`runtime`]：节点目录、执行器与运行状态文件的装配。
//! - [`wire`]：把分片执行器接入共享 YY 线服务的后端与启动参数。
//! - [`client`]：远程客户端（即 `yyds-client`）。
//! - 其余能力按 `database` 门面的同一命名再导出。

#![deny(missing_docs)]

pub mod runtime;
pub mod wire;

/// 远程客户端门面。
pub mod client {
    pub use yyds_client::*;
}

pub use database::{cluster, embed, execution, formats, input, migration, protocol, query, server, storage, types, udf, harness};
