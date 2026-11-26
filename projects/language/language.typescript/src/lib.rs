//! TypeScript 语言前端：真词法／语法、AST → HIR → MIR 阶段链、JS 语义
//! 解释器与 OVM 第一投影。
//!
//! 阶段链：[`syntax`] 产 AST（`language.typescript.ir`，纯语法树）→
//! [`hir`] 作用域树 / 绑定身份 / 函数表 → [`mir`] 带类型事实与语义类别的
//! 语言中间表示。解释器（[`runtime`]）与特化（[`specialize`]，Futamura
//! 第一投影）从同一 MIR 分派：解释器执行 MIR 作差分基线与语义来源，
//! 特化把 MIR 残化为目标无关的 [`residual`] IR，再由 [`ovm`] 适配器做
//! 残差 → OVM LIR 的纯结构映射。目标侧不解释 TS 语义；truthiness、`+`
//! 转换、`===` 身份、文本格式化全部裁决在语言侧：静态可判定的在特化期
//! 结算进产物，动态不可判定的走 `odin.host` 模块下内核未收录的 `ts.*`
//! provider 符号，由 [`runtime::provider::TsStdProvider`] 实现与解释器
//! 同源的语义。

pub mod formatter;
pub mod hir;
pub mod mir;
pub mod runtime;
pub mod syntax;

#[cfg(feature = "ovm-lane")]
pub mod ovm;
#[cfg(feature = "ovm-lane")]
pub mod residual;
#[cfg(feature = "ovm-lane")]
pub mod specialize;

use language_typescript_ir::Program;
use runtime::{Interpreter, RuntimeError};
use syntax::SyntaxError;

/// 解析一段 TypeScript 子集源码为 AST 程序。
pub fn parse(source: &str) -> Result<Program, SyntaxError> {
    syntax::parse_program(source)
}

/// AST → HIR（作用域树 / 绑定身份 / 函数表；全函数降低）。
pub fn lower_hir(program: &Program) -> hir::Hir {
    hir::lower(program)
}

/// HIR → MIR（类型事实与语义类别附着）。
pub fn lower_mir(hir: &hir::Hir) -> mir::MirProgram {
    mir::lower(hir)
}

/// MIR → 解释执行（差分基线；顶层已执行）。
pub fn interpret(program: &mir::MirProgram) -> Result<Interpreter, RuntimeError> {
    Interpreter::from_mir(program)
}

/// MIR → 目标无关残差 IR（第一投影特化）。
#[cfg(feature = "ovm-lane")]
pub fn specialize(program: &mir::MirProgram) -> Result<residual::ResidualProgram, specialize::SpecializeFailure> {
    specialize::specialize(program)
}

/// 残差 IR → OVM `.odin` 产物（纯结构映射适配器）。
#[cfg(feature = "ovm-lane")]
pub fn to_odin(residual: &residual::ResidualProgram, module_name: &str) -> Result<ovm::OvmArtifact, specialize::SpecializeFailure> {
    ovm::to_odin(residual, module_name)
}

/// 解析并解释执行源码，返回装载完成的解释器（含 `console.log` trace）。
pub fn run(source: &str) -> Result<Interpreter, RunError> {
    let program = parse(source)?;
    Interpreter::new(&program).map_err(RunError::Runtime)
}

/// `run` 的结构化失败：前端错误或运行时错误。
#[derive(Debug)]
pub enum RunError {
    /// 词法／语法失败。
    Syntax(SyntaxError),
    /// 解释执行失败。
    Runtime(RuntimeError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax(error) => write!(formatter, "前端错误: {error}"),
            Self::Runtime(error) => write!(formatter, "运行时错误: {error}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<SyntaxError> for RunError {
    fn from(error: SyntaxError) -> Self {
        Self::Syntax(error)
    }
}

impl From<RuntimeError> for RunError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// TypeScript × OVM 第一投影：源码 → `.odin` 产物（含取证信息）。
#[cfg(feature = "ovm-lane")]
pub fn specialize_to_odin(source: &str, module_name: &str) -> Result<ovm::OvmArtifact, specialize::SpecializeFailure> {
    let program = parse(source).map_err(|error| specialize::SpecializeFailure::Unsupported {
        construct: "前端解析".to_string(),
        detail: error.to_string(),
    })?;
    ovm::specialize_to_odin(&program, module_name)
}
