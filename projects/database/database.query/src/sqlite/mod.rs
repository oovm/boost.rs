//! SQLite 方言的查询前端:词法、AST、`parse` 与结构化拒绝。
//!
//! 本模块拥有 SQLite 语法子集的解析与不可变 AST(无执行期状态),
//! 供 `sqlite` 产品门面与下游绑定复用。宿主自定义函数的解析期派发
//! 通过 [`frontend::HostFunctionArity`] 这一最小视图接入,前端不依赖
//! 任何宿主注册表实现。
//!
//! - [`frontend`] 是 tokenizer + AST + `parse` 的完整前端;
//! - [`error`] 是解析与执行共享的结构化拒绝 [`error::SqlError`];
//! - [`value`] 是宿主可见的单元格、字面量与记录解码;
//! - [`catalog`] 是 schema 对象形状与 catalog 容器;
//! - [`bind`] 是名称/列解析后的表达式与关系形状;
//! - [`json_string`] 是门面 JSON 信封共用的字符串转义。

pub mod bind;
pub mod catalog;
pub mod error;
pub mod frontend;
pub mod value;

pub use error::{SqlError, storage_failure};

/// 把一段文本转义为 JSON 字符串字面量(含两侧引号)。
///
/// 引擎与错误信封都手工拼装 JSON,转义口径必须一致,因此集中在此处。
pub fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", character as u32)),
            character => out.push(character),
        }
    }
    out.push('"');
    out
}
