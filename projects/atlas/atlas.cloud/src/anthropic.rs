//! Anthropic messages：`POST {base}/v1/messages`。
//!
//! 请求体固定 `model`、`max_tokens`（必填）与 `messages`，系统消息合进顶层 `system`
//! 字段而不是放进 `messages`；认证是 `x-api-key` 加 `anthropic-version`。不做流式与工具调用。
//!
//! # 解码与分层
//!
//! - 2xx 且 `content` 数组里至少有一个 `type == "text"` 的块 → [`ChatOutcome::Completed`]，
//!   文本按块顺序拼接，`usage.input_tokens` / `usage.output_tokens` 归一成 [`TokenUsage`]。
//! - 2xx 但结构不符合契约 → [`ChatOutcome::Indeterminate`]：请求确实送达过，不能伪造成成功。
//! - 非 2xx 且错误体是 `{"type":"error","error":{"type","message"}}` → [`ChatOutcome::Rejected`]，
//!   `code` 取 `error.type`。
//! - 非 2xx 但错误体无法解析 → [`ChatOutcome::Rejected`]，`code` 由状态派生为 `HTTP_<status>`。
//! - 传输层失败 → [`CloudError::Indeterminate`]（本地构造失败为 [`CloudError::Transport`]）。
//!
//! 客户端单次发送，不重试、不做退避；`429` 的 `Retry-After` 只作为结果字段回传。
//!
//! ```no_run
//! use atlas_cloud::{
//!     ai::{ChatMessage, ChatOutcome, ChatRole},
//!     anthropic::Client,
//!     tencent::CloudError,
//! };
//!
//! fn run() -> Result<(), CloudError> {
//!     let client = Client::new("sk-ant-example", "https://api.example.com");
//!     let messages = [ChatMessage { role: ChatRole::User, content: "你好".into() }];
//!     match client.chat("claude-example", &messages, 256)? {
//!         ChatOutcome::Completed { text, .. } => println!("{text}"),
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
use serde_json::{Map, Value, json};

use crate::{
    ai::{ChatMessage, ChatOutcome, ChatRole, TokenUsage, counter, parse_body, request_id, retry_after},
    error::{body_snippet, map_transport_error},
    tencent::CloudError,
};

/// messages 接口的路径；`base_url` 只给到站点根，路径在这里固定。
const PATH: &str = "/v1/messages";
/// 接口版本头；版本不对时上游会直接拒绝。
const VERSION: &str = "2023-06-01";

/// Anthropic messages 客户端。
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
    /// `base_url` 形如 `https://api.example.com`，末尾的 `/` 会被去掉；`/v1/messages` 由
    /// [`Client::endpoint`] 补齐。
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
    /// 纯函数：只依赖入参，不读时钟、不联网，可以直接断言序列化结果。系统消息按出现顺序
    /// 用空行合并进顶层 `system`；顶层不出现 `system` 键时说明没有系统消息。
    pub fn encode_request(&self, model: &str, messages: &[ChatMessage], max_tokens: u32) -> String {
        let mut system = Vec::new();
        let mut turns = Vec::new();
        for message in messages {
            match message.role {
                ChatRole::System => system.push(message.content.as_str()),
                role => turns.push(json!({ "role": role.as_wire(), "content": message.content })),
            }
        }

        let mut body = Map::new();
        body.insert("model".to_string(), json!(model));
        body.insert("max_tokens".to_string(), json!(max_tokens));
        if !system.is_empty() {
            body.insert("system".to_string(), json!(system.join("\n\n")));
        }
        body.insert("messages".to_string(), Value::Array(turns));
        Value::Object(body).to_string()
    }

    /// 发送一次 messages 请求，单次调用，不重试。
    ///
    /// 只要拿到响应就返回 `Ok`：拒绝与不确定都是结果而不是错误。只有本地构造失败或传输层
    /// 失败才返回 [`CloudError`]。
    pub fn chat(&self, model: &str, messages: &[ChatMessage], max_tokens: u32) -> Result<ChatOutcome, CloudError> {
        let url = Url::parse(&self.endpoint()).map_err(|error| CloudError::Transport(error.to_string()))?;
        let request = Request::new(Method::Post, url)
            .with_header("content-type", "application/json")
            .with_header("x-api-key", self.api_key.clone())
            .with_header("anthropic-version", VERSION)
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
        let request_id = request_id(headers, &["request-id", "x-request-id"], parsed.as_ref());

        if !(200..300).contains(&status) {
            let (code, message) = error_parts(parsed.as_ref(), body, status);
            return ChatOutcome::Rejected { status, code, message, request_id, retry_after: retry_after(headers) };
        }

        let Some(blocks) = parsed.as_ref().and_then(|value| value.get("content")).and_then(Value::as_array)
        else {
            return ChatOutcome::Indeterminate { reason: "HTTP 2xx 但缺少 content 数组".to_string(), request_id };
        };
        let text: String = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        if text.is_empty() {
            return ChatOutcome::Indeterminate { reason: "HTTP 2xx 但 content 里没有 text 块".to_string(), request_id };
        }
        let usage = TokenUsage {
            input_tokens: counter(parsed.as_ref().and_then(|value| value.pointer("/usage/input_tokens"))),
            output_tokens: counter(parsed.as_ref().and_then(|value| value.pointer("/usage/output_tokens"))),
        };
        ChatOutcome::Completed { text, usage, request_id }
    }
}

/// 从错误体里取 `(code, message)`。
///
/// 错误体形状是 `{"type":"error","error":{"type","message"}}`，`code` 取 `error.type`；
/// 整段不是这个结构时按 HTTP 状态派生 `code`，`message` 取响应体片段。
fn error_parts(parsed: Option<&Value>, body: &[u8], status: u16) -> (String, String) {
    let error = parsed.and_then(|value| value.get("error"));
    let code = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| format!("HTTP_{status}"), ToString::to_string);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| body_snippet(body), ToString::to_string);
    (code, message)
}
