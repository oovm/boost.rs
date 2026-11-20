//! Valkyrie 语言的编译实现（`language.valkyrie`）。
//!
//! # 归属
//!
//! 语言含义与编译事实的生产者归具体语言：类型模型、HIR/MIR、类型检查、诊断与目标适配都在这里。
//! 与语言无关的链接、优化、发射和目标解析在 `compiler.*` 的其它 crate；面向编辑器/CLI 的
//! 文档与请求服务归 `language.*`，不由本 crate 承担。
//!
//! # 依赖方向
//!
//! - 消费 `compiler.*`、Acorn 格式合同，以及解析本语言源码所需的 `data.text` 语法 crate。
//! - 不反向要求 `compiler.*` 认识本语言的语法、效果或运行时语义。
//!
//! # 已迁出
//!
//! 产品工具链（`legion` 编排与 `asgard` 交付）已移入 `package.manager.legion`：
//! 它们是 Valkyrie 生态的产品策略与 CLI，不属于语言编译事实。本 crate 只保留编译本身。

#![feature(new_range_api)]
#![warn(missing_docs)]

#[macro_use]
pub mod utilities;
pub mod compiler;
pub mod formatter;
