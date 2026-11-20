//! TypeScript 语言服务（`language.typescript` 前端层）。
//!
//! 语法底座（`ast/builder/language/lexer/parser/printer/scope`）留在 `data-text-typescript`；
//! 本 crate 拥有源码格式化（CST token-gap）。formatter 由 [`formatter::format_source`] 直接消费，
//! 不注册进 `language-format` 平台（与迁移前状态一致）。
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod formatter;
