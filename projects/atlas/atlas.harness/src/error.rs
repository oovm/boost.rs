//! 脚本与替身自身的结构化错误。
//!
//! 这里描述的是「回放脚本或 fixture 不成立」，不是云厂商的返回结果：它们不会出现在
//! [`crate::Diagnosis`] 里，也不会被折叠成 [`atlas_cloud::tencent::CloudError`]。

use std::fmt::{Display, Formatter};

/// harness 自身的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// fixture 某一行不是合法的回放响应。
    Fixture {
        /// 1 起始的行号。
        line: usize,
        /// 失败原因。
        detail: String,
    },
    /// 回放脚本已经用完，但仍有请求到达。
    ReplayExhausted {
        /// 期望的响应序号，1 起始。
        expected: usize,
        /// 到达请求的方法。
        method: String,
        /// 到达请求的 URL。
        url: String,
    },
    /// 响应头不合法，无法构造回放响应。
    InvalidHeader {
        /// 头名。
        name: String,
        /// 失败原因。
        detail: String,
    },
}

impl Display for HarnessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fixture { line, detail } => write!(formatter, "fixture 第 {line} 行解析失败：{detail}"),
            Self::ReplayExhausted { expected, method, url } => {
                write!(formatter, "回放脚本已耗尽：期望第 {expected} 个响应但收到 {method} {url}")
            }
            Self::InvalidHeader { name, detail } => write!(formatter, "响应头 `{name}` 非法：{detail}"),
        }
    }
}

impl std::error::Error for HarnessError {}
