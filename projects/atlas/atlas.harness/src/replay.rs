//! 用脚本化响应驱动真实的 `atlas-cloud` 客户端。
//!
//! [`Replay`] 按脚本顺序返回响应，并把到达的请求保存成 [`RecordedRequest`]，供调用方断言
//! 签名头与请求体。脚本用完时显式报错，绝不返回空响应。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use boost_net::http::{Headers, Request, Response, Transport};
use serde_json::Value;

use crate::error::HarnessError;

/// 一条预先录制的 HTTP 响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CannedResponse {
    status: u16,
    headers: Headers,
    body: Vec<u8>,
}

impl CannedResponse {
    /// 用状态码与响应文本构造，自动补 `content-type: application/json`。
    pub fn json(status: u16, body: &str) -> Self {
        let mut headers = Headers::new();
        headers.set("content-type", "application/json");
        Self { status, headers, body: body.as_bytes().to_vec() }
    }

    /// 设置一个响应头；同名头被替换。
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.set(name, value);
        self
    }
}

/// 回放时看到的请求快照。
///
/// 只覆盖排障与断言需要的事实：方法、URL、全部请求头与请求体原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    method: String,
    url: String,
    headers: Headers,
    body: String,
}

impl RecordedRequest {
    /// 从真实请求抓取快照。
    fn capture(request: &Request) -> Self {
        Self {
            method: request.method.as_str().to_string(),
            url: request.url.to_string(),
            headers: request.headers.clone(),
            body: String::from_utf8_lossy(&request.body).into_owned(),
        }
    }

    /// 请求方法，例如 `POST`。
    pub fn method(&self) -> &str {
        &self.method
    }

    /// 请求 URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 按大小写不敏感的头名取第一个值。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// 返回全部请求头，重复头保留全部值。
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// 请求体原文；非 UTF-8 字节按替换字符呈现。
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// 顺序回放脚本化响应的传输替身。
///
/// 克隆得到的是同一份脚本的共享句柄，因此可以同时交给客户端与断言代码：客户端持有克隆，
/// 测试保留原值读取 [`Replay::last_request`] 与 [`Replay::remaining`]。
#[derive(Debug, Clone)]
pub struct Replay {
    inner: Arc<Inner>,
}

/// 共享的脚本、游标与最近一次请求。
#[derive(Debug)]
struct Inner {
    responses: Vec<CannedResponse>,
    cursor: AtomicUsize,
    last: Mutex<Option<RecordedRequest>>,
}

impl Replay {
    /// 用脚本化响应构造；顺序即请求到达顺序。
    pub fn new(responses: Vec<CannedResponse>) -> Self {
        Self { inner: Arc::new(Inner { responses, cursor: AtomicUsize::new(0), last: Mutex::new(None) }) }
    }

    /// 从 JSONL 文本构造：每行一个 `{status, headers, body}` 对象。
    ///
    /// 空行被忽略；`headers` 可省略，`body` 必须存在且是字符串。解析失败时返回带 1 起始
    /// 行号的 [`HarnessError::Fixture`]，头名或头值非法时返回
    /// [`HarnessError::InvalidHeader`]。
    pub fn from_jsonl(text: &str) -> Result<Self, HarnessError> {
        let mut responses = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            responses.push(parse_response(line, index + 1)?);
        }
        Ok(Self::new(responses))
    }

    /// 返回尚未消费的响应数量。
    pub fn remaining(&self) -> usize {
        self.inner.responses.len().saturating_sub(self.inner.cursor.load(Ordering::SeqCst))
    }

    /// 返回最近一次到达的请求快照；还没有请求时为 `None`。
    pub fn last_request(&self) -> Option<RecordedRequest> {
        let last = self.inner.last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        last.clone()
    }

    /// 取出并前移游标；脚本用完时返回带位置与请求信息的错误。
    fn next_response(&self, request: &Request) -> Result<CannedResponse, HarnessError> {
        let index = self.inner.cursor.fetch_add(1, Ordering::SeqCst);
        self.inner.responses.get(index).cloned().ok_or_else(|| HarnessError::ReplayExhausted {
            expected: index + 1,
            method: request.method.as_str().to_string(),
            url: request.url.to_string(),
        })
    }
}

impl Transport for Replay {
    fn send(&self, request: &Request) -> Result<Response, boost_net::NetError> {
        {
            let mut last = self.inner.last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *last = Some(RecordedRequest::capture(request));
        }
        let canned = self.next_response(request).map_err(|error| boost_net::NetError::Transport(error.to_string()))?;
        Ok(Response { url: request.url.clone(), status: canned.status, headers: canned.headers, body: canned.body, redirects: Vec::new() })
    }
}

/// 解析一行 fixture。
fn parse_response(line: &str, number: usize) -> Result<CannedResponse, HarnessError> {
    let value: Value = serde_json::from_str(line).map_err(|error| HarnessError::Fixture { line: number, detail: error.to_string() })?;
    let object = value.as_object().ok_or_else(|| HarnessError::Fixture { line: number, detail: "每行必须是 JSON 对象".to_string() })?;
    let status = object
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| HarnessError::Fixture { line: number, detail: "缺少合法的 `status`（0..=65535）".to_string() })?;
    let body = object
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| HarnessError::Fixture { line: number, detail: "缺少字符串 `body`".to_string() })?;
    let mut response = CannedResponse::json(status, body);
    if let Some(headers) = object.get("headers") {
        let headers =
            headers.as_object().ok_or_else(|| HarnessError::Fixture { line: number, detail: "`headers` 必须是对象".to_string() })?;
        for (name, value) in headers {
            let value = value
                .as_str()
                .ok_or_else(|| HarnessError::Fixture { line: number, detail: format!("头 `{name}` 的值必须是字符串") })?;
            validate_header(name, value)?;
            response = response.with_header(name, value);
        }
    }
    Ok(response)
}

/// 校验头名是 HTTP token，头值不含换行。
fn validate_header(name: &str, value: &str) -> Result<(), HarnessError> {
    let valid_name = !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if !valid_name {
        return Err(HarnessError::InvalidHeader { name: name.to_string(), detail: "头名必须是 HTTP token".to_string() });
    }
    if value.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err(HarnessError::InvalidHeader { name: name.to_string(), detail: "头值不得包含换行".to_string() });
    }
    Ok(())
}
