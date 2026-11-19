#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

//! Valkyrie 语言消费门面。
//!
//! 底层能力分别归 `compiler.valkyrie`、`package.manager` 与 `interpreter.vm.ovm`；
//! 本 crate 只提供统一入口与面向宿主的稳定结构化 API，不复制任何底层实现。

/// Valkyrie 编译事实与稳定编译 API。
pub mod compiler {
    pub use compiler_valkyrie::compiler::*;
}

/// Asgard 交付与静态托管。
pub mod asgard {
    pub use package_manager::asgard::*;
}

/// Legion 工程编排。
pub mod legion {
    pub use package_manager::legion::*;
}

pub mod execution;
pub mod ovm;
pub mod product;
