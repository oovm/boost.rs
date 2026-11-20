//! AWSL 语言实现：模板降级、ABI 索引、Render IR，以及静态渲染栈
//! （样式/布局合同、HTML 渲染、SSG 页面装配）。
//!
//! 本 crate 只覆盖语言层语义；交付工具链（WASM 岛、部署、发布、宿主产物）
//! 在 `package.manager.asgard`。
#![deny(missing_docs)]

#[macro_use]
extern crate language_valkyrie;

pub mod awsl;
pub mod codegen;
pub mod error;
pub mod layout;
pub mod ssg;
pub mod style;

pub use error::{AwslError, AwslResult};
