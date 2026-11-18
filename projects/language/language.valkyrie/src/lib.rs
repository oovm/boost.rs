//! Valkyrie 语言实现树。
//!
//! # 树位置
//!
//! `language.*` 拥有具体语言的语义与本语言专用合同。它不是通用编译器能力：`compiler.*` 只承接
//! 与语言无关的链接、优化与发射，`language.*` 才拥有类型模型、HIR/MIR、类型检查与目标适配。
//!
//! # 依赖方向
//!
//! - 允许消费 `compiler.*`（类型身份、事实合同、发射与目标解析）与 Acorn 的格式合同。
//! - 语言前端需要解析自己的源码时允许消费 Oak 语法 crate；这条例外只适用于 `language.*`，
//!   不适用于 `compiler.*`。
//! - 不反向要求 `compiler.*` 认识本语言的语法、效果或运行时语义。

#![deny(missing_docs)]

pub mod compiler;
