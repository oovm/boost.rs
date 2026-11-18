use std::{fmt, sync::Arc, thread, time::Duration};

use boost_net::http::{HttpTransport, HttpTransportConfig, Method, ProxyConfig, Request, Response, Transport};
use serde::de::DeserializeOwned;
use url::Url;

use crate::{RegistryError, RetryConfig};

/// 响应正文预算：npm 与 .nupkg 制品可能远大于默认协议层上限。
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// 注册表适配器共享的 HTTP 响应视图。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应正文。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 状态码是否在 2xx 范围。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 把正文按 UTF-8 有损解码为文本。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 把正文解析为 JSON。
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RegistryError> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

impl From<Response> for HttpResponse {
    fn from(response: Response) -> Self {
        Self { status: response.status, body: response.body }
    }
}

/// 注册表适配器共享的 HTTP 调用面。
///
/// 默认使用 [`HttpTransport`] 的同步 HTTP/1.1 传输；需要代理、DNS 或连接池策略的宿主
/// 用 [`Self::with_transport`] 注入自己的 [`Transport`]。
#[derive(Clone)]
pub struct HttpClient {
    transport: Option<Arc<dyn Transport + Send + Sync>>,
    proxy: Option<ProxyConfig>,
    retry: RetryConfig,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("injected_transport", &self.transport.is_some())
            .field("proxy", &self.proxy)
            .field("retry", &self.retry)
            .finish()
    }
}

enum AuthHeader<'a> {
    None,
    Bearer(&'a str),
    Raw(&'a str),
}

impl HttpClient {
    /// 以给定重试策略创建客户端。
    pub fn new(retry: RetryConfig) -> Result<Self, RegistryError> {
        Ok(Self { transport: None, proxy: None, retry })
    }

    /// 以显式代理策略创建客户端。
    pub fn with_proxy(retry: RetryConfig, proxy: ProxyConfig) -> Self {
        Self { transport: None, proxy: Some(proxy), retry }
    }

    /// 注入宿主传输实现，忽略内置 [`HttpTransport`] 与代理配置。
    pub fn with_transport(transport: Arc<dyn Transport + Send + Sync>, retry: RetryConfig) -> Self {
        Self { transport: Some(transport), proxy: None, retry }
    }

    /// 返回显式代理策略。
    pub fn proxy(&self) -> Option<&ProxyConfig> {
        self.proxy.as_ref()
    }

    /// 返回重试策略。
    pub fn retry(&self) -> &RetryConfig {
        &self.retry
    }

    /// 解析代理环境变量，供宿主决定是否注入代理传输。
    pub fn proxy_env_hint() -> Option<String> {
        for (name, value) in [
            ("HTTPS_PROXY", std::env::var("HTTPS_PROXY").ok()),
            ("https_proxy", std::env::var("https_proxy").ok()),
            ("HTTP_PROXY", std::env::var("HTTP_PROXY").ok()),
            ("http_proxy", std::env::var("http_proxy").ok()),
            ("ALL_PROXY", std::env::var("ALL_PROXY").ok()),
            ("all_proxy", std::env::var("all_proxy").ok()),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                return Some(format!("{name}={value}"));
            }
        }
        None
    }

    /// GET 并解析 JSON；404 映射为 [`RegistryError::NotFound`]。
    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, RegistryError> {
        let response = self.send(Method::Get, url, AuthHeader::None, &[], None, "application/json", false)?;
        if response.status == 404 {
            return Err(RegistryError::NotFound(url.to_string()));
        }
        if !response.is_success() {
            return Err(RegistryError::status(response.status, response.text()));
        }
        response.json()
    }

    /// GET 并返回原始字节。
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        let response = self.send(Method::Get, url, AuthHeader::None, &[], None, "application/json", false)?;
        if !response.is_success() {
            return Err(RegistryError::status(response.status, response.text()));
        }
        Ok(response.body)
    }

    /// 携带 Bearer 令牌 GET 并解析 JSON。
    pub fn get_authenticated_json<T: DeserializeOwned>(&self, url: &str, token: &str) -> Result<T, RegistryError> {
        let response = self.send(Method::Get, url, AuthHeader::Bearer(token), &[], None, "application/json", false)?;
        if !response.is_success() {
            return Err(RegistryError::status(response.status, response.text()));
        }
        response.json()
    }

    /// 携带额外请求头 GET。
    pub fn get_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, RegistryError> {
        self.send(Method::Get, url, AuthHeader::None, headers, None, "application/json", false)
    }

    /// 携带显式 `Authorization` 值与额外请求头 GET。
    pub fn get_authenticated_with_headers(
        &self,
        url: &str,
        authorization: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, RegistryError> {
        self.send(Method::Get, url, AuthHeader::Raw(authorization), headers, None, "application/json", false)
    }

    /// PUT 原始正文；`token` 以 Bearer 形式发送。
    pub fn put_bytes(&self, url: &str, body: &[u8], token: Option<&str>, content_type: &str) -> Result<HttpResponse, RegistryError> {
        let auth = token.map(AuthHeader::Bearer).unwrap_or(AuthHeader::None);
        self.send(Method::Put, url, auth, &[], Some(body), content_type, true)
    }

    /// POST 原始正文并携带显式 `Authorization` 值。
    pub fn post_bytes_with_authorization(
        &self,
        url: &str,
        body: &[u8],
        authorization: &str,
        content_type: &str,
    ) -> Result<HttpResponse, RegistryError> {
        self.send(Method::Post, url, AuthHeader::Raw(authorization), &[], Some(body), content_type, true)
    }

    /// POST 原始正文，不带认证头（例如公共查询接口）。
    pub fn post_body(&self, url: &str, body: &[u8], content_type: &str) -> Result<HttpResponse, RegistryError> {
        self.send(Method::Post, url, AuthHeader::None, &[], Some(body), content_type, true)
    }

    /// POST 原始正文，`token` 以 Bearer 形式发送。
    pub fn post_bytes(&self, url: &str, body: &[u8], token: &str, content_type: &str) -> Result<HttpResponse, RegistryError> {
        self.send(Method::Post, url, AuthHeader::Bearer(token), &[], Some(body), content_type, true)
    }

    fn send(
        &self,
        method: Method,
        url: &str,
        auth: AuthHeader<'_>,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        content_type: &str,
        upload: bool,
    ) -> Result<HttpResponse, RegistryError> {
        let parsed = Url::parse(url).map_err(|error| RegistryError::message(format!("invalid registry URL `{url}`: {error}")))?;
        let mut delay = self.retry.initial_delay_ms;
        let mut last_error = None;

        for attempt in 0..=self.retry.max_retries {
            let mut request = Request::new(method.clone(), parsed.clone());
            if method != Method::Get {
                request.set_header("content-type", content_type);
            }
            match auth {
                AuthHeader::None => {}
                AuthHeader::Bearer(token) => request.set_header("authorization", format!("Bearer {token}")),
                AuthHeader::Raw(value) => request.set_header("authorization", value),
            }
            for (name, value) in headers {
                request.set_header(*name, *value);
            }
            if let Some(body) = body {
                request.body = body.to_vec();
            }

            match self.transport_for(upload).send(&request) {
                Ok(response) => {
                    let retryable = response.status == 429 || (500..600).contains(&response.status);
                    if retryable && attempt < self.retry.max_retries {
                        thread::sleep(Duration::from_millis(delay));
                        delay = next_delay(delay, &self.retry);
                        last_error = Some(RegistryError::status(response.status, "retryable status"));
                        continue;
                    }
                    return Ok(response.into());
                }
                Err(error) => {
                    if attempt < self.retry.max_retries {
                        thread::sleep(Duration::from_millis(delay));
                        delay = next_delay(delay, &self.retry);
                        last_error = Some(RegistryError::from(error));
                        continue;
                    }
                    return Err(RegistryError::from(error));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| RegistryError::message("retry exhausted")))
    }

    /// 每次请求按上传/普通区分整链超时；注入传输时超时策略由宿主负责。
    fn transport_for(&self, upload: bool) -> TransportHandle<'_> {
        if let Some(transport) = &self.transport {
            return TransportHandle::Shared(transport.as_ref());
        }
        let seconds = if upload { self.retry.upload_timeout_secs } else { self.retry.request_timeout_secs };
        TransportHandle::Owned(HttpTransport::with_config(HttpTransportConfig {
            max_body_bytes: MAX_BODY_BYTES,
            timeout: Duration::from_secs(seconds.max(1)),
            proxy: self.proxy.clone(),
            ..HttpTransportConfig::default()
        }))
    }
}

enum TransportHandle<'a> {
    Shared(&'a dyn Transport),
    Owned(HttpTransport),
}

impl TransportHandle<'_> {
    fn send(&self, request: &Request) -> Result<Response, boost_net::NetError> {
        match self {
            Self::Shared(transport) => transport.send(request),
            Self::Owned(transport) => transport.send(request),
        }
    }
}

fn next_delay(delay: u64, retry: &RetryConfig) -> u64 {
    ((delay as f64) * retry.backoff_multiplier).min(retry.max_delay_ms as f64) as u64
}

/// 拼接 `{endpoint}/{path}`，忽略两端多余斜杠。
pub fn build_url(endpoint: &str, path: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    format!("{endpoint}/{path}")
}

/// 查询值的最小百分号编码。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 返回代理环境变量的简短提示，供宿主在错误信息中使用。
pub fn proxy_env_hint() -> Option<String> {
    HttpClient::proxy_env_hint()
}
