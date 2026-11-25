//! Valkyrie × 机器码目标适配：直接消费语言 MIR，产出目标私有 x86-64 输入。
//!
//! 机器码 lane 不再经 `FragmentSubmission` / `TargetExecutionPlan`：lane 无关的
//! 链接、布局与导出事实由 [`super::lane::LaneContext`] 一次结算（五条后端 lane
//! 共享），本模块只保留机器码 lane 的函数级 lowering 与三个交付入口，读取已验证
//! 的 Valkyrie MIR 与表示计划，产出目标私有的 [`X64ProgramInput`] /
//! [`X64LibraryProgramInput`] / [`X64ExtensionProgramInput`]，再交 emitter 的
//! 装配服务编码为镜像。JVM/CLR/Wasm/OVM 仍走 `target_plan` 的公共 IR 路径，
//! 与本模块无关。

pub(crate) mod exe;
pub(crate) mod extension;
pub(crate) mod library;
pub(crate) mod lowerer;
