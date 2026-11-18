//! 优化底座的最小中立名称合同。
//!
//! 只包含 E-graph 重写需要的符号身份：标识符片段、限定名与能力标签。
//! 语言侧的类型、evidence、布局与诊断规则不在此处，仍由各语言持有。

use std::{fmt, sync::Arc};

/// 一个合法标识符片段。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Identifier {
    text: Arc<str>,
}

impl Identifier {
    /// 创建一个标识符。
    pub fn new(text: &str) -> Self {
        Self { text: Arc::from(text) }
    }

    /// 返回标识符文本。
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

/// 一个已经完成绑定的完全限定名称。
///
/// 表示名称查找、`import` 展开与歧义消解都结束后的稳定结果；
/// 待解析路径是不同语义对象，不能直接充当限定名使用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QualifiedName {
    parts: Vec<Identifier>,
}

impl QualifiedName {
    /// 从已解析的标识符片段创建限定名。
    pub fn new(parts: Vec<Identifier>) -> Self {
        Self { parts }
    }

    /// 返回限定名的各级片段。
    pub fn parts(&self) -> &[Identifier] {
        self.parts.as_ref()
    }

    /// 返回是否为空名称。
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<&str> = self.parts.iter().map(|i| i.as_str()).collect();
        write!(f, "{}", parts.join("::"))
    }
}

/// 分析、优化与目标选择共享的稳定能力标签。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CapabilityTag(String);

impl CapabilityTag {
    /// 新建能力标签。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 返回标签字符串切片。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CapabilityTag {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CapabilityTag {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for CapabilityTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
