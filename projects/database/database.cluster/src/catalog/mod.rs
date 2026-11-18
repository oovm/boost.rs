//! 目录生命周期、分片放置与所有权 fencing。

mod catalog;
mod format;
/// 节点启动时的 catalog 选择与校验。
pub mod node;

pub use catalog::{Catalog, catalog_path, is_catalog_path};
pub use format::{MAGIC, decode, encode};
pub use node::{CatalogLoadError, LOCAL_SHARD_ID, NodeCatalogFacts, NodeRouting, load_node_catalog};
