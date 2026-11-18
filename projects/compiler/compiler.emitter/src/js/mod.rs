//! JS/Web 文本目标的通用文本原语。
//!
//! 只含与框架无关的文本处理：TypeScript 类型擦除、token 级压缩与 source map 片段。
//! 具体框架语义（组件、模板、路由）不在本模块；宿主按需调用这些原语。

mod erase;
mod print;

pub use erase::{erase_types, erase_types_with_deletions};
pub use print::{SourceLines, SourceMapSegment, map_erased_offset, minify_source, source_map_json};
