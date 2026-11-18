//! OpenAI 兼容的 chat completions：`POST {base}/v1/chat/completions`。
//!
//! 覆盖 OpenAI 与采用同一请求/响应形状的兼容服务商。请求体只有 `model`、`max_tokens` 与
//! `messages`，不做流式、工具调用与采样参数；认证是 `Authorization: Bearer <key>`。
//!
//! # 解码与分层
//!
//! - 2xx 且 `choices[0].message.content` 是字符串 → [`ChatOutcome::Completed`]。
//! - 2xx 但结构不符合契约 → [`ChatOutcome::Indeterminate`]：请求确实送达过，不能伪造成成功。
//! - 非 2xx 且错误体是 `{"error":{"message","type","code"}}` → [`ChatOutcome::Rejected`]，
//!   `code` 取 `error.code`（缺失用 `error.type`）。
//! - 非 2xx 但错误体无法解析 → [`ChatOutcome::Rejected`]，`code` 由状态派生为 `HTTP_<status>`。
//! - 传输层失败 → [`CloudError::Indeterminate`]（本地构造失败为 [`CloudError::Transport`]）。
//!
//! 客户端单次发送，不重试、不做退避；`429` 的 `Retry-After` 只作为结果字段回传，等待与
//! 重试由调用方决定。
//!
//! ```no_run
//! use atlas_cloud::{
//!     ai::{ChatMessage, ChatOutcome, ChatRole},
//!     openai::Client,
//!     tencent::CloudError,
//! };
//!
//! fn run() -> Result<(), CloudError> {
//!     let client = Client::new("sk-example", "https://api.example.com");
//!     let messages = [ChatMessage { role: ChatRole::User, content: "你好".into() }];
//!     match client.chat("gpt-example", &messages, 256)? {
//!         ChatOutcome::Completed { text, usage, .. } => {
//!             println!("{text}（{} token）", usage.output_tokens)
//!         }
//!         ChatOutcome::Rejected { code, .. } => println!("拒绝：{code}"),
//!         ChatOutcome::Indeterminate { reason, .. } => println!("结果未知：{reason}"),
//!     }
//!     Ok(())
//! }
//! ```

use boost_net::{
    Url,
    http::{Headers, HttpTransport, Method, Request, Transport},
};
use serde_json::{Value, json};

use crate::{
    ai::{ChatMessage, ChatOutcome, TokenUsage, counter, parse_body, request_id, retry_after},
    error::{body_snippet, map_transport_error},
    tencent::CloudError,
};

/// 兼容接口的路径；`base_url` 只给到站点根，路径在这里固定。
const PATH: &str = "/v1/chat/completions";

/// OpenAI 兼容 chat 客户端。
///
/// `api_key` 与 `base_url` 在构造时固定；没有手写 [`std::fmt::Debug`]，密钥不会随日志落盘。
pub struct Client {
    api_key: String,
    base_url: String,
    transport: Box<dyn Transport + Send + Sync>,
}

impl Client {
    /// 用密钥与站点根地址构造客户端。
    ///
    /// `base_url` 形如 `https://api.example.com`，末尾的 `/` 会被去掉；`/v1/chat/completions`
    /// 由 [`Client::endpoint`] 补齐。
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), base_url: base_url.into().trim_end_matches('/').to_string(), transport: Box::new(HttpTransport::new()) }
    }

    /// 替换底层传输。
    ///
    /// 供测试替身、受控回放或自建代理使用；默认是 [`HttpTransport`]。
    pub fn with_transport(mut self, transport: Box<dyn Transport + Send + Sync>) -> Self {
        self.transport = transport;
        self
    }

    /// 完整的请求地址。
    pub fn endpoint(&self) -> String {
        format!("{}{PATH}", self.base_url)
    }

    /// 构造请求体。
    ///
    /// 纯函数：只依赖入参，不读时钟、不联网，可以直接断言序列化结果。
    pub fn encode_request(&self, model: &str, messages: &[ChatMessage], max_tokens: u32) -> String {
        let messages = messages.iter().map(|message| json!({ "role": message.role.as_wire(), "content": message.content })).collect::<Vec<_>>();
        json!({ "model": model, "max_tokens": max_tokens, "messages": messages }).to_string()
    }

    /// 发送一次 chat 请求，单次调用，不重试。
    ///
    /// 只要拿到响应就返回 `Ok`：拒绝与不确定都是结果而不是错误。只有本地构造失败或传输层
    /// 失败才返回 [`CloudError`]。
    pub fn chat(&self, model: &str, messages: &[ChatMessage], max_tokens: u32) -> Result<ChatOutcome, CloudError> {
        let url = Url::parse(&self.endpoint()).map_err(|error| CloudError::Transport(error.to_string()))?;
        let request = Request::new(Method::Post, url)
            .with_header("content-type", "application/json")
            .with_header("authorization", format!("Bearer {}", self.api_key))
            .with_body(self.encode_request(model, messages, max_tokens).into_bytes());
        let response = self.transport.send(&request).map_err(map_transport_error)?;
        Ok(Self::interpret_response(response.status, &response.headers, &response.body))
    }

    /// 把一次 HTTP 响应映射为 chat 结果。
    ///
    /// 传输入口只把「拿到响应」的情况交到这里；传输失败直接归入
    /// [`CloudError::Indeterminate`]，两者不能互相折叠。
    pub fn interpret_response(status: u16, headers: &Headers, body: &[u8]) -> ChatOutcome {
        let parsed = parse_body(body);
        let request_id = request_id(headers, &["x-request-id"], parsed.as_ref());

        if !(200..300).contains(&status) {
            let (code, message) = error_parts(parsed.as_ref(), body, status);
            return ChatOutcome::Rejected { status, code, message, request_id, retry_after: retry_after(headers) };
        }

        let Some(text) = parsed.as_ref().and_then(|value| value.pointer("/choices/0/message/content")).and_then(Value::as_str)
        else {
            return ChatOutcome::Indeterminate { reason: "HTTP 2xx 但缺少可用的 choices[0].message.content".to_string(), request_id };
        };
        let usage = TokenUsage {
            input_tokens: counter(parsed.as_ref().and_then(|value| value.pointer("/usage/prompt_tokens"))),
            output_tokens: counter(parsed.as_ref().and_then(|value| value.pointer("/usage/completion_tokens"))),
        };
        ChatOutcome::Completed { text: text.to_string(), usage, request_id }
    }
}

/// 从错误体里取 `(code, message)`。
///
/// 错误体形状是 `{"error":{"message","type","code"}}`；`code` 可能是 `null`，此时退回
/// `type`。整段不是 JSON 时按 HTTP 状态派生 `code`，`message` 取响应体片段。
fn error_parts(parsed: Option<&Value>, body: &[u8], status: u16) -> (String, String) {
    let error = parsed.and_then(|value| value.get("error"));
    let code = error
        .and_then(|error| {
            error
                .get("code")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| error.get("type").and_then(Value::as_str).filter(|value| !value.is_empty()))
        })
        .map_or_else(|| format!("HTTP_{status}"), ToString::to_string);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| body_snippet(body), ToString::to_string);
    (code, message)
}
