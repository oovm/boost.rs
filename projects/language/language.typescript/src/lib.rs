//! TypeScript 语言前端：真词法／语法、JS 语义解释器与 OVM 第一投影。
//!
//! 分层：AST →（语言自己的）解释语义 → 语言 × 目标适配器 → 目标 builder /
//! 编码器 → loader。目标侧不解释 TS 语义；truthiness、`+` 转换、`===` 身份、
//! 文本格式化全部裁决在语言侧：静态可判定的在特化期结算进产物，动态不可
//! 判定的走 `odin.host` 模块下内核未收录的 `ts.*` provider 符号，由
//! [`runtime::provider::TsStdProvider`] 实现与解释器同源的语义。

pub mod formatter;
pub mod runtime;
pub mod syntax;

#[cfg(feature = "ovm-lane")]
pub mod ovm;
#[cfg(feature = "ovm-lane")]
pub mod specialize;

use language_typescript_ir::Program;
use runtime::{Interpreter, RuntimeError};
use syntax::SyntaxError;

/// 解析一段 TypeScript 子集源码为 IR 程序。
pub fn parse(source: &str) -> Result<Program, SyntaxError> {
    syntax::parse_program(source)
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
