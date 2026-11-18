//! Mongo 持久化记录编码：集合 catalog 记录、索引条目与 `local.oplog.rs` 条目形态。
//!
//! 这里只决定字节/JSON 布局，不知道后端是 `.yykv` 还是 KV 执行器；两个后端都把
//! 集合与索引写成同一份 [`record::collection_record`] 记录，因此切换后端不需要
//! 重写元数据，也不会出现第二套 catalog 编码。

/// `local.oplog.rs` 条目形态与渲染。
pub mod oplog;
/// 集合选项与索引条目的 catalog 记录编码。
pub mod record;

pub use oplog::OplogEntry;
pub use record::{
    CATALOG_DOCUMENT_VERSION, CollectionOptions, INDEX_NAMESPACE_MARKER, IndexSpec, StoreError, collection_options_json, collection_record,
    index_namespace, is_index_namespace, parse_collection_record, parse_collections, parse_indexes,
};
