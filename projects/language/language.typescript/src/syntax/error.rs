//! 词法／语法错误的结构化表示。

use std::fmt;

/// 前端错误：带确切行列与期望形状的结构化语法错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 错误说明。
    pub message: String,
    /// 期望出现的构造（失败闭合时给出确切缺项，不做猜测回退）。
    pub expected: String,
    /// 1 起算的行号。
    pub line: usize,
    /// 1 起算的列号（UTF-8 码点列）。
    pub column: usize,
}

impl SyntaxError {
    /// 构造一个结构化前端错误。
    pub fn new(message: impl Into<String>, expected: impl Into<String>, line: usize, column: usize) -> Self {
        Self { message: message.into(), expected: expected.into(), line, column }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "第 {} 行第 {} 列：{}（期望 {}）", self.line, self.column, self.message, self.expected)
    }
}

impl std::error::Error for SyntaxError {}
