#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! 语言无关的残化接收端合同（Futamura 第一投影目标）。
//!
//! # 分层
//!
//! - 本 crate 只定义**接收端接口**：特化后的操作流如何被消费方接收。
//! - 具体语言的解释器特化（把解释器 match 分支折叠为操作流）归该语言前端。
//! - 目标产物生成（例如机器码或 PE 容器）归目标发射器或产品侧接收端实现，
//!   不在本 crate 内。
//! - 未知调用必须作为残余操作保留，不能在特化期执行或丢弃。

pub mod residual;

pub use residual::ResidualSink;
