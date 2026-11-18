//! AI 协议族的共享作者面。
//!
//! 只承载所有 AI chat 协议都要用的词汇：一次请求里的角色与消息 [`ChatMessage`]、计费单位
//! [`TokenUsage`]、结果三态 [`ChatOutcome`]，以及两个协议共用的响应解码辅助。协议差异
//! （认证头、系统消息位置、错误体形状）留在 [`crate::openai`] 与 [`crate::anthropic`]。
//!
//! # 结果语义
//!
//! chat 类接口多数没有幂等键，超时就是未知结果：请求可能已经生成并计费。因此
//! [`ChatOutcome::Indeterminate`] 与 [`ChatOutcome::Rejected`] 不能折叠，
//! 前者**不得**自动重试，后者中只有 `429` 属于「按 `Retry-After` 后重试」的情形。

use boost_net::http::Headers;
use serde_json::Value;

use crate::tencent::RequestId;

/// 一次调用消耗的 token 数。
///
/// 两个协议只在字段名上不同（`prompt_tokens` / `input_tokens` 等），这里统一成同一对
/// 计数；缺失的计数按 `0` 处理，不伪造成「已统计」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入侧 token 数。
    pub input_tokens: u64,
    /// 输出侧 token 数。
    pub output_tokens: u64,
}

/// 一条消息的角色。
///
/// 只覆盖 chat 协议共有的三种角色；工具调用、函数结果等扩展角色不在首发范围内。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    /// 系统提示。
    System,
    /// 用户输入。
    User,
    /// 模型上一轮回复。
    Assistant,
}

impl ChatRole {
    /// 协议里的角色拼写。
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// 一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// 消息角色。
    pub role: ChatRole,
    /// 消息正文。
    pub content: String,
}

/// 一次 chat 调用的结果。
///
/// 三态不可折叠：`Completed` 表示拿到了可用的回复，`Rejected` 表示服务端明确拒绝，
/// `Indeterminate` 表示请求已经送达但无法判定是否已生成，可能已经计费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatOutcome {
    /// 服务端返回了正文与用量。
    Completed {
        /// 回复正文。
        text: String,
        /// token 用量。
        usage: TokenUsage,
        /// 请求 ID，用于对账。
        request_id: Option<RequestId>,
    },
    /// 服务端明确拒绝；`429` 的 `retry_after` 来自 `Retry-After` 头。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 厂商错误码；无法解析错误体时由状态派生为 `HTTP_<status>`。
        code: String,
        /// 厂商错误描述；无法解析错误体时取响应体片段。
        message: String,
        /// 请求 ID；网关类错误可能没有。
        request_id: Option<RequestId>,
        /// `Retry-After` 的秒数；缺失或无法解析时为 `None`。
        retry_after: Option<u64>,
    },
    /// 请求已送达但结果无法判定，调用方不得自动重试。
    Indeterminate {
        /// 无法判定的原因。
        reason: String,
        /// 请求 ID；传输阶段失败时通常没有。
        request_id: Option<RequestId>,
    },
}

impl ChatOutcome {
    /// 返回服务端是否给出了可用回复。
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// 按顺序取第一个非空的请求 ID 头，缺失时退回响应体的 `id`。
///
/// 两个协议的头名不同（`x-request-id` 与 `request-id`），但优先级规则一致：响应头是
/// 网关与厂商都稳定回传的位置，响应体 `id` 只是兜底。
pub(crate) fn request_id(headers: &Headers, header_names: &[&str], body: Option<&Value>) -> Option<RequestId> {
    header_names
        .iter()
        .find_map(|name| headers.get(name).filter(|value| !value.is_empty()).map(RequestId::new))
        .or_else(|| body.and_then(|value| value.get("id")).and_then(Value::as_str).filter(|value| !value.is_empty()).map(RequestId::new))
}

/// `Retry-After` 的秒数。
///
/// 只接受整秒；缺失、非数字或超出 `u64` 时返回 `None`，不猜默认值。
pub(crate) fn retry_after(headers: &Headers) -> Option<u64> {
    headers.get("retry-after")?.trim().parse().ok()
}

/// 取一个计数型字段；缺失或不是无符号整数时按 `0` 处理。
pub(crate) fn counter(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

/// 把响应体解析成任意 JSON 值；不是合法 JSON 时返回 `None`。
pub(crate) fn parse_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}
