//! VON 语言服务（`language.von` 前端层）。
//!
//! 语法底座（`ast/builder/language/lexer/parser/printer`）留在 `data-text-von`；
//! 本 crate 拥有语言服务：CST token-gap 源码格式化，并向 `language-format` 平台注册 provider。
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod formatter;

pub use formatter::register_source_formatter;
