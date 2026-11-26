//! Valkyrie × 机器码目标适配：直接消费语言 MIR，产出目标私有 x86-64 输入。
//!
//! 机器码 lane 不再经公共 IR 中间计划：lane 无关的
//! 链接、布局与导出事实由 [`super::lane::LaneContext`] 一次结算（五条后端 lane
//! 共享），本模块只保留机器码 lane 的函数级 lowering 与三个交付入口，读取已验证
//! 的 Valkyrie MIR 与表示计划，产出目标私有的 [`X64ProgramInput`] /
//! [`X64LibraryProgramInput`] / [`X64ExtensionProgramInput`]，再交 emitter 的
//! 装配服务编码为镜像。JVM/CLR/Wasm/OVM 同样消费 [`super::lane::LaneContext`]
//! 直出各自目标私有输入，与本模块无关。

pub(crate) mod exe;
pub(crate) mod extension;
pub(crate) mod library;
pub(crate) mod lowerer;
/// x86-64 语言 × 目标适配器：链接分类、槽载体判定与 operator 冻结裁决。
pub(crate) mod operator;
