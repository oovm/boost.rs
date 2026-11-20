//! TypeScript 语言服务（`language.typescript` 前端层）。
//!
//! 语法底座（`ast/builder/language/lexer/parser/printer/scope`）留在 `data-text-typescript`；
//! 本 crate 拥有源码格式化（CST token-gap）与语言服务接缝（hover、模块级 definition/references、
//! 全文格式化）。formatter 由 [`formatter::format_source`] 直接消费，不注册进 `language-format` 平台；
//! LSP/MCP 只包装 [`lsp`] 提供的同一套语义。
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Source formatter over TypeScript CST token gaps.
pub mod formatter;
/// Language service contract implementations (hover, navigation, formatting).
pub mod lsp;
/// MCP server adapter over [`lsp`].
pub mod mcp;
