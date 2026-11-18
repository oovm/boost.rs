//! Valkyrie 语言的编译实现（`compiler.valkyrie`）。
//!
//! # 归属
//!
//! 语言含义与编译事实的生产者归具体语言：类型模型、HIR/MIR、类型检查、诊断与目标适配都在这里。
//! 与语言无关的链接、优化、发射和目标解析在 `compiler.*` 的其它 crate；面向编辑器/CLI 的
//! 文档与请求服务归 `language.*`，不由本 crate 承担。
//!
//! # 依赖方向
//!
//! - 消费 `compiler.*`、Acorn 格式合同，以及解析本语言源码所需的 Oak 语法 crate。
//! - 不反向要求 `compiler.*` 认识本语言的语法、效果或运行时语义。
//!
//! # 进行中
//!
//! 交付侧（`asgard`）与编排（`legion`）文件已随迁入，但它们依赖仍在产品仓的
//! `nyar-runner` / `nyar-workspace` / `nyar-package-manager`；这些迁入 `package.*` / 工程服务前，
//! 两个模块不参与编译。

#![allow(missing_docs)]

pub mod compiler;
