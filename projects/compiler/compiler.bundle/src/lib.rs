//! 打包链的中立算法层：模块记录、身份解析、模块图、语句级链接与分包计划。
//!
//! # 归属
//!
//! 本 crate 拥有与语言无关的打包算法：
//!
//! - [`module`]：模块记录类型与「源码 → 事实」的来源合同（实现由语言适配层提供）。
//! - [`resolve`]：specifier + 上下文 → 模块身份的确定性规则。
//! - [`graph`]：把记录与解析粘成从入口可达的模块图。
//! - [`link`]：语句级活跃不动点。
//! - [`chunk`]：保留集合上的确定性多入口分包计划。
//! - [`source`]：按已解析身份读取源码与资源字节的只读端口（宿主注入）。
//!
//! 不归本 crate 的内容：
//!
//! - JS/TS 语法解析与事实提取（`ModuleFactSource` 的实现，归对应语言适配层）。
//! - 最终文本装配、类型擦除、压缩与文件写出（`compiler.emitter` 的 JS/Web 文本目标与宿主）。
//! - 工程发现、监听、依赖安装（`package.*` 与工程服务）。

#![deny(missing_docs)]

pub mod chunk;
pub mod diag;
pub mod emit;
pub mod graph;
pub mod link;
pub mod module;
pub mod resolve;
pub mod source;

pub use diag::PackDiagnostic;
pub use module::ModuleFactSource;
pub use source::{SourceReader, StdSourceReader};
