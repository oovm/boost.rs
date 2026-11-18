//! 包发布机制（`package.publish`）。
//!
//! 拥有打包、发布流水线与发布目标解析：
//!
//! - 产物打包：`package/` 前缀与扁平两种布局、忽略规则、`files` glob、侧车 manifest。
//! - 发布流水线：版本递增 → 打包 → 渠道上传 → 可选 git tag。
//! - 发布目标解析：`publish[].target` → `dist/{identity}`，目标选择不受运输渠道改写。
//!
//! 不归本 crate：版本求解与 lock（`package.resolve`）、渠道请求（`package.registry`）、
//! 缓存（`package.cache`）、发布资格与访问控制（宿主策略）。

#![deny(missing_docs)]

mod error;
mod pack;
mod publish;
mod publish_target;
mod scripts;

pub use error::{PublishError, Result};
pub use pack::{
    PackMeta, PackResult, PackageIgnore, RegistryBinEntry, RegistryPackOptions, bin_name_from_logical, glob_match, pack,
    pack_registry_artifact, unpack,
};
pub use publish::PackagePublisher;
pub use publish_target::{ResolvedPublishArtifact, resolve_publish_artifact};
pub use scripts::{ScriptResult, ScriptRunner};
