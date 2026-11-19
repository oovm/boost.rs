//! 包缓存机制（`package.cache`）。
//!
//! 两个互不相同的缓存面：
//!
//! - [`PackageCache`]：已安装包的本地目录索引（下载落盘后复用，离线安装的来源）。
//! - [`WorkspaceCache`]：内容寻址的制品桶存储，键是内容哈希，条目带类型标签与格式版本。
//!
//! 两者都只做存取与失效，不做版本选择、来源决定或发布；那些归 `package.resolve` 与
//! `package.publish`。缓存根目录由调用方给出，不假定产品数据目录。

#![warn(missing_docs)]

mod error;
mod hash;
mod package;
mod path;
mod store;
mod von;

pub use error::{CacheError, Result};
pub use hash::{combined_hash, file_hash, files_hash};
pub use package::PackageCache;
pub use path::{resolve_marker_root, sanitize_bucket_name};
pub use store::WorkspaceCache;
