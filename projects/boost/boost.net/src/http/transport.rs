use std::{
    io::{self, BufReader, Read, Write},
    time::{Duration, Instant},
};

use crate::{
    NetError,
    connection::{self, Connection, DeadlineSocket},
    tls::TlsConfig,
};

use url::Url;

use super::{Headers, Method, Redirect, Request, Response, Transport, is_token};

/// 配置内置 HTTP/1.1 传输。
#[derive(Debug, Clone)]
pub struct HttpTransportConfig {
    /// 解码 transfer framing 之后的响应正文上限。
    pub max_body_bytes: usize,
    /// 最多跟随的重定向次数。
    pub max_redirects: u32,
    /// 整条请求链的超时。
    ///
    /// 流式请求（[`HttpTransport::send_stream`]）只把它用于连接、发送与等待响应头；
    /// 响应正文的等待由 [`Self::stream_idle_timeout`] 约束。
    pub timeout: Duration,
    /// 流式响应正文每次读取的空闲超时；零值表示一直阻塞。
    pub stream_idle_timeout: Duration,
    /// 响应头块的上限。
    pub max_header_bytes: usize,
    /// `https` URL 允许的 TLS 协议版本。
    pub tls: TlsConfig,
    /// 显式配置的代理；`None` 表示直连。
    ///
    /// 传输层不读取 `HTTPS_PROXY` 一类环境变量：是否采用环境代理是调用方策略，
    /// 需要时用 [`ProxyConfig::from_env`] 自行取值后填在这里。
    pub proxy: Option<ProxyConfig>,
}

impl Default for HttpTransportConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 8 * 1024 * 1024,
            max_redirects: 8,
            timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(300),
            max_header_bytes: 64 * 1024,
            tls: TlsConfig::default(),
            proxy: None,
        }
    }
}

/// 显式代理策略：一个 HTTP 代理地址与可选的直连名单。
///
/// `http` 目标经代理发送 absolute-form 请求行，`https` 目标先建立 `CONNECT` 隧道再握手 TLS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    endpoint: Url,
    no_proxy: Vec<String>,
}

impl ProxyConfig {
    /// 解析 `http://[user:password@]host:port` 形式的代理地址。
    pub fn parse(input: &str) -> Result<Self, NetError> {
        let endpoint = Url::parse(input.trim()).map_err(|error| NetError::Unsupported(format!("invalid proxy URL `{input}`: {error}")))?;
        if endpoint.scheme() != "http" {
            return Err(NetError::Unsupported(format!("proxy scheme `{}` is not HTTP", endpoint.scheme())));
        }
        if endpoint.host_str().is_none() || endpoint.port().is_none() {
            return Err(NetError::Unsupported("proxy URL needs an explicit host and port".into()));
        }
        if !matches!(endpoint.path(), "" | "/") || endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err(NetError::Unsupported("proxy URL must not carry a path, query or fragment".into()));
        }
        Ok(Self { endpoint, no_proxy: Vec::new() })
    }

    /// 从 `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` 取值；这是显式调用才生效的策略。
    pub fn from_env() -> Result<Option<Self>, NetError> {
        for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            let Ok(value) = std::env::var(name)
            else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() || value.eq_ignore_ascii_case("none") {
                continue;
            }
            let mut config = Self::parse(value)?;
            if let Ok(no_proxy) = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")) {
                config = config.with_no_proxy(no_proxy.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string));
            }
            return Ok(Some(config));
        }
        Ok(None)
    }

    /// 追加直连条目：完整主机名、`.suffix` 后缀或 `*`。
    pub fn with_no_proxy(mut self, entries: impl IntoIterator<Item = String>) -> Self {
        self.no_proxy.extend(entries);
        self
    }

    /// 代理地址。
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// 直连名单。
    pub fn no_proxy(&self) -> &[String] {
        &self.no_proxy
    }

    /// 该目标主机是否应跳过代理。
    pub fn bypasses(&self, host: &str) -> bool {
        let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
        self.no_proxy.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            if entry == "*" {
                return true;
            }
            match entry.strip_prefix('.') {
                Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
                None => host == entry,
            }
        })
    }

    /// `Proxy-Authorization` 头取值；代理地址不含用户信息时为 `None`。
    fn authorization(&self) -> Option<String> {
        let username = self.endpoint.username();
        if username.is_empty() {
            return None;
        }
        let credentials = format!("{}:{}", username, self.endpoint.password().unwrap_or_default());
        Some(format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, credentials)))
    }
}

/// Minimal synchronous HTTP/1.1 transport.
#[derive(Debug, Clone, Default)]
pub struct HttpTransport {
    /// Transport limits and timeout policy.
    pub config: HttpTransportConfig,
}

impl HttpTransport {
    /// Create a transport with conservative defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a transport with explicit limits.
    pub fn with_config(config: HttpTransportConfig) -> Self {
        Self { config }
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Request) -> Result<Response, NetError> {
        let started = Instant::now();
        let (mut current_url, mut method, mut headers, mut body) = prepare(request)?;
        let mut redirects = Vec::new();

        for _ in 0..=self.config.max_redirects {
            let remaining = remaining_timeout(self.config.timeout, started.elapsed())?;
            let mut response = self.send_once(&current_url, &method, &headers, &body, remaining)?;
            if self.config.max_redirects > 0 && is_redirect(response.status) {
                if redirects.len() >= self.config.max_redirects as usize {
                    return Err(NetError::Transport(format!("redirect chain exceeded {} hops", self.config.max_redirects)));
                }
                let location =
                    response.headers.get("location").ok_or_else(|| NetError::Transport("redirect without location header".into()))?;
                let mut next_url = current_url.join(location).map_err(|error| NetError::Transport(error.to_string()))?;
                next_url.set_fragment(None);
                validate_redirect(&current_url, &next_url)?;
                if current_url.origin() != next_url.origin() {
                    remove_sensitive_headers(&mut headers);
                }
                if should_switch_to_get(response.status, &method) {
                    method = Method::Get;
                    body.clear();
                    remove_entity_headers(&mut headers);
                }
                redirects.push(Redirect { url: current_url, status: response.status });
                current_url = next_url;
                continue;
            }
            response.url = current_url;
            response.redirects = redirects;
            return Ok(response);
        }
        Err(NetError::Transport("redirect loop exhausted".into()))
    }
}

impl HttpTransport {
    /// 发送请求并返回一个可增量读取的响应。
    ///
    /// 与 [`Transport::send`] 的区别只在于正文：响应头到达后立即返回，调用方用自己的缓冲区
    /// 逐段读取。正文按 `Content-Length`、`chunked` 或连接关闭（EOF）三种 framing 增量解码，
    /// 累计字节数仍受 [`HttpTransportConfig::max_body_bytes`] 约束。
    ///
    /// 语义约定：
    ///
    /// - 连接、发送与等待响应头使用 [`HttpTransportConfig::timeout`]；
    /// - 正文每次读取使用 [`HttpTransportConfig::stream_idle_timeout`] 作为空闲超时；
    /// - 对端在中途断开返回 [`NetError::Body`]，不会静默当成正常结束；
    /// - [`StreamResponse`] 被丢弃即关闭连接，这是取消流式响应的唯一方式。
    pub fn send_stream(&self, request: &Request) -> Result<StreamResponse, NetError> {
        let started = Instant::now();
        let (mut current_url, mut method, mut headers, mut body) = prepare(request)?;
        let mut redirects = Vec::new();

        for _ in 0..=self.config.max_redirects {
            let remaining = remaining_timeout(self.config.timeout, started.elapsed())?;
            let mut response = self.send_stream_once(&current_url, &method, &headers, &body, remaining)?;
            if self.config.max_redirects > 0 && is_redirect(response.status) {
                if redirects.len() >= self.config.max_redirects as usize {
                    return Err(NetError::Transport(format!("redirect chain exceeded {} hops", self.config.max_redirects)));
                }
                let location =
                    response.headers.get("location").ok_or_else(|| NetError::Transport("redirect without location header".into()))?;
                let mut next_url = current_url.join(location).map_err(|error| NetError::Transport(error.to_string()))?;
                next_url.set_fragment(None);
                validate_redirect(&current_url, &next_url)?;
                if current_url.origin() != next_url.origin() {
                    remove_sensitive_headers(&mut headers);
                }
                if should_switch_to_get(response.status, &method) {
                    method = Method::Get;
                    body.clear();
                    remove_entity_headers(&mut headers);
                }
                redirects.push(Redirect { url: current_url, status: response.status });
                current_url = next_url;
                continue;
            }
            response.url = current_url;
            response.redirects = redirects;
            return Ok(response);
        }
        Err(NetError::Transport("redirect loop exhausted".into()))
    }

    fn send_once(&self, url: &url::Url, method: &Method, headers: &Headers, body: &[u8], timeout: Duration) -> Result<Response, NetError> {
        let proxy = self.proxy_for(url)?;
        let mut connection = self.open_connection(url, timeout, proxy)?;
        write_request(&mut connection, url, method, headers, body, proxy)?;
        read_response(&mut BufReader::new(connection), url.clone(), method, self.config.max_header_bytes, self.config.max_body_bytes)
    }

    fn send_stream_once(
        &self,
        url: &url::Url,
        method: &Method,
        headers: &Headers,
        body: &[u8],
        timeout: Duration,
    ) -> Result<StreamResponse, NetError> {
        let proxy = self.proxy_for(url)?;
        let mut connection = self.open_connection(url, timeout, proxy)?;
        write_request(&mut connection, url, method, headers, body, proxy)?;
        let mut reader = BufReader::new(connection);
        let (status, headers) = read_response_head(&mut reader, self.config.max_header_bytes)?;
        let framing = response_framing(method, status, &headers, self.config.max_body_bytes)?;
        let body =
            ResponseBody::new(reader, framing, self.config.max_body_bytes, self.config.max_header_bytes, self.config.stream_idle_timeout);
        Ok(StreamResponse { url: url.clone(), status, headers, redirects: Vec::new(), body })
    }

    /// 选出 URL 实际要走的代理；`no_proxy` 命中的目标返回 `None`。
    fn proxy_for<'a>(&'a self, url: &url::Url) -> Result<Option<&'a ProxyConfig>, NetError> {
        let host = url.host_str().ok_or_else(|| NetError::Unsupported("URL has no host".into()))?;
        let network_host = host.trim_start_matches('[').trim_end_matches(']');
        Ok(self.config.proxy.as_ref().filter(|proxy| !proxy.bypasses(network_host)))
    }

    /// 建立到目标的连接，必要时先穿过代理隧道并完成 TLS 握手。
    fn open_connection(&self, url: &url::Url, timeout: Duration, proxy: Option<&ProxyConfig>) -> Result<Connection<DeadlineSocket>, NetError> {
        let host = url.host_str().ok_or_else(|| NetError::Unsupported("URL has no host".into()))?;
        let network_host = host.trim_start_matches('[').trim_end_matches(']');
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| NetError::Unsupported("timeout is too large".into()))?;
        let port = url.port_or_known_default().ok_or_else(|| NetError::Unsupported("URL has no port".into()))?;
        let (dial_host, dial_port) = match proxy {
            Some(proxy) => {
                let proxy_host = proxy.endpoint().host_str().ok_or_else(|| NetError::Unsupported("proxy URL has no host".into()))?;
                let proxy_port = proxy.endpoint().port().ok_or_else(|| NetError::Unsupported("proxy URL has no port".into()))?;
                (proxy_host.to_string(), proxy_port)
            }
            None => (network_host.to_string(), port),
        };
        let tcp = connection::dial(&dial_host, dial_port, deadline)?;
        let mut socket = DeadlineSocket::new(tcp, deadline);
        if let Some(proxy) = proxy.filter(|_| url.scheme() == "https") {
            tunnel_through_proxy(&mut socket, network_host, port, proxy.authorization().as_deref(), self.config.max_header_bytes)?;
        }
        connection::secure(url.scheme(), network_host, socket, &self.config.tls)
    }
}

/// 校验请求并复制出重定向过程中会改写的工作副本。
fn prepare(request: &Request) -> Result<(url::Url, Method, Headers, Vec<u8>), NetError> {
    validate_url(&request.url)?;
    if !is_token(request.method.as_str()) {
        return Err(NetError::Unsupported("invalid HTTP method".into()));
    }
    let mut current_url = request.url.clone();
    current_url.set_fragment(None);
    let mut headers = Headers::new();
    for (name, value) in request.headers.iter() {
        validate_header(name, value)?;
        headers.append(name, value);
    }
    Ok((current_url, request.method.clone(), headers, request.body.clone()))
}

/// 写出一条请求的头部与正文。
fn write_request<S: Write>(
    connection: &mut S,
    url: &url::Url,
    method: &Method,
    headers: &Headers,
    body: &[u8],
    proxy: Option<&ProxyConfig>,
) -> Result<(), NetError> {
    let host = url.host_str().ok_or_else(|| NetError::Unsupported("URL has no host".into()))?;
    let path = match url.query() {
        Some(query) => format!("{}?{query}", if url.path().is_empty() { "/" } else { url.path() }),
        None if url.path().is_empty() => "/".to_string(),
        None => url.path().to_string(),
    };
    // 明文 HTTP 经代理走 absolute-form；HTTPS 已建立隧道，仍用 origin-form。
    let target = if proxy.is_some() && url.scheme() == "http" { url.as_str().to_string() } else { path };
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let mut wire = format!("{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method.as_str(), target, host_header);
    if headers.get_all("content-length").len() > 1 {
        return Err(NetError::Unsupported("request has more than one Content-Length header".into()));
    }
    for (name, value) in headers.iter() {
        validate_header(name, value)?;
        if matches!(name, "transfer-encoding" | "expect" | "upgrade") {
            return Err(NetError::Unsupported("request streaming, expectation and upgrade headers are not supported".into()));
        }
        if name == "host" || name == "connection" {
            continue;
        }
        if name == "content-length" && value.parse::<usize>().ok() != Some(body.len()) {
            return Err(NetError::Unsupported("request Content-Length does not match body".into()));
        }
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    if let Some(authorization) = proxy.filter(|_| url.scheme() == "http").and_then(ProxyConfig::authorization) {
        wire.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    if !headers.contains("content-length") {
        wire.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    wire.push_str("\r\n");
    connection.write_all(wire.as_bytes()).map_err(NetError::Body)?;
    connection.write_all(body).map_err(NetError::Body)?;
    connection.flush().map_err(NetError::Body)
}

/// 正文 framing 方式。
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// 没有正文（`HEAD`、`204`、`304`）。
    Empty,
    /// 恰好 `Content-Length` 字节。
    Length(usize),
    /// `chunked` 分块编码。
    Chunked(ChunkState),
    /// 读到对端关闭连接为止。
    UntilClose,
}

/// 分块解码中跨读取保留的状态。
#[derive(Debug, Clone, Copy)]
struct ChunkState {
    /// 当前块还剩多少字节。
    remaining: usize,
    /// 当前块的数据已读完，还差一个 `CRLF`。
    needs_terminator: bool,
}

/// A response whose body is still arriving.
///
/// 由 [`HttpTransport::send_stream`] 返回；正文拥有底层连接，因此丢弃它就会关闭 socket，
/// 对端随之收到取消。正文的 framing 与长度限制和整包 [`Response`] 一致。
#[derive(Debug)]
pub struct StreamResponse {
    /// URL after transport-level redirects.
    pub url: Url,
    /// Numeric HTTP status code.
    pub status: u16,
    /// Response headers with case-insensitive names, including repeated fields.
    pub headers: Headers,
    /// Redirect responses followed before this response.
    pub redirects: Vec<Redirect>,
    /// 尚未读完的响应正文。
    pub body: ResponseBody,
}

impl StreamResponse {
    /// Return whether the status code is in the successful `2xx` range.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Return whether the status code is in the redirect `3xx` range.
    pub fn is_redirection(&self) -> bool {
        (300..400).contains(&self.status)
    }

    /// Return whether the status code is in the client or server error range.
    pub fn is_error(&self) -> bool {
        self.status >= 400
    }
}

/// 增量解码的响应正文，拥有底层连接。
///
/// 读取语义：
///
/// - 完整读完返回 `Ok(0)`，之后继续读取仍是 `Ok(0)`；
/// - 对端在声明的长度或分块完成前断开返回 [`NetError::Body`]（`UnexpectedEof`），
///   不会把截断当成正常结束；
/// - 累计字节数超过 `max_body_bytes` 返回 [`NetError::BodyTooLarge`]；
/// - 每次读取都受空闲超时约束，超时返回 [`NetError::Body`]。
pub struct ResponseBody {
    reader: BufReader<Connection<DeadlineSocket>>,
    framing: Framing,
    limit: usize,
    metadata_remaining: usize,
    consumed: usize,
    done: bool,
}

impl core::fmt::Debug for ResponseBody {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ResponseBody")
            .field("framing", &self.framing)
            .field("limit", &self.limit)
            .field("consumed", &self.consumed)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ResponseBody {
    fn new(
        mut reader: BufReader<Connection<DeadlineSocket>>,
        framing: Framing,
        limit: usize,
        max_header_bytes: usize,
        idle_timeout: Duration,
    ) -> Self {
        // 响应头已经到达，正文改用空闲超时，长连接不会被整体时限切断。
        reader.get_mut().use_idle(idle_timeout);
        Self { reader, framing, limit, metadata_remaining: max_header_bytes, consumed: 0, done: false }
    }

    /// 返回一个可从其他线程强制关闭连接的句柄。
    ///
    /// 读取线程可能正阻塞在 `read` 上，仅在持有者一侧丢弃正文无法立刻唤醒它；
    /// 跨线程取消（例如客户端断开）应先调用 [`CancelHandle::cancel`]。
    pub fn cancel_handle(&self) -> Result<CancelHandle, NetError> {
        self.reader.get_ref().canceller().map(CancelHandle).map_err(NetError::Body)
    }

    /// 读取下一段正文；`Ok(0)` 表示正文已经完整结束。
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, NetError> {
        self.read_framed(buffer)
    }

    fn read_framed(&mut self, buffer: &mut [u8]) -> Result<usize, NetError> {
        if buffer.is_empty() || self.done {
            return Ok(0);
        }
        loop {
            match self.framing {
                Framing::Empty => {
                    self.done = true;
                    return Ok(0);
                }
                Framing::UntilClose => {
                    let read = self.reader.read(buffer).map_err(NetError::Body)?;
                    if read == 0 {
                        self.done = true;
                    }
                    self.consume(read)?;
                    return Ok(read);
                }
                Framing::Length(remaining) => {
                    if remaining == 0 {
                        self.done = true;
                        return Ok(0);
                    }
                    let wanted = remaining.min(buffer.len());
                    let read = self.reader.read(&mut buffer[..wanted]).map_err(NetError::Body)?;
                    if read == 0 {
                        return Err(NetError::Body(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed before the declared Content-Length",
                        )));
                    }
                    self.framing = Framing::Length(remaining - read);
                    return Ok(read);
                }
                Framing::Chunked(mut state) => {
                    if state.remaining == 0 {
                        if state.needs_terminator {
                            let mut terminator = [0u8; 2];
                            self.reader.read_exact(&mut terminator).map_err(NetError::Body)?;
                            if terminator != *b"\r\n" {
                                return Err(NetError::Transport("invalid chunk terminator".into()));
                            }
                            state.needs_terminator = false;
                        }
                        let line = self.read_metadata_line()?;
                        let size_text = line.split(';').next().unwrap_or("");
                        if size_text.is_empty() || !size_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                            return Err(NetError::Transport("invalid chunk size".into()));
                        }
                        let size = usize::from_str_radix(size_text, 16).map_err(|_| NetError::Transport("invalid chunk size".into()))?;
                        if size == 0 {
                            self.consume_trailers()?;
                            self.framing = Framing::Chunked(state);
                            self.done = true;
                            return Ok(0);
                        }
                        if self.consumed.saturating_add(size) > self.limit {
                            return Err(NetError::BodyTooLarge { limit: self.limit });
                        }
                        state.remaining = size;
                        self.framing = Framing::Chunked(state);
                        continue;
                    }
                    let wanted = state.remaining.min(buffer.len());
                    let read = self.reader.read(&mut buffer[..wanted]).map_err(NetError::Body)?;
                    if read == 0 {
                        return Err(NetError::Body(io::Error::new(io::ErrorKind::UnexpectedEof, "chunk truncated")));
                    }
                    state.remaining -= read;
                    if state.remaining == 0 {
                        state.needs_terminator = true;
                    }
                    self.framing = Framing::Chunked(state);
                    self.consume(read)?;
                    return Ok(read);
                }
            }
        }
    }

    fn consume(&mut self, count: usize) -> Result<(), NetError> {
        self.consumed = self.consumed.saturating_add(count);
        if self.consumed > self.limit {
            return Err(NetError::BodyTooLarge { limit: self.limit });
        }
        Ok(())
    }

    fn read_metadata_line(&mut self) -> Result<String, NetError> {
        let limit = self.metadata_remaining.min(4096);
        let line = read_line(&mut self.reader, limit)?;
        self.metadata_remaining = self.metadata_remaining.saturating_sub(line.len() + 2);
        Ok(line)
    }

    fn consume_trailers(&mut self) -> Result<(), NetError> {
        loop {
            let trailer = self.read_metadata_line()?;
            if trailer.is_empty() {
                return Ok(());
            }
            let (name, value) = trailer.split_once(':').ok_or_else(|| NetError::Transport("invalid trailer".into()))?;
            validate_header(name, value)?;
            if matches!(name.to_ascii_lowercase().as_str(), "content-length" | "transfer-encoding" | "host") {
                return Err(NetError::Transport("invalid framing trailer".into()));
            }
        }
    }
}

impl Read for ResponseBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_framed(buffer).map_err(|error| match error {
            // 保留下层 I/O 错误种类（例如 `UnexpectedEof`），不要压平成 `Other`。
            NetError::Body(error) => error,
            other => io::Error::other(other),
        })
    }
}

/// 从其他线程取消流式响应的句柄。
///
/// 调用 [`Self::cancel`] 会关闭底层连接：读取线程立即得到错误，不必等空闲超时。
#[derive(Debug)]
pub struct CancelHandle(connection::CancelHandle);

impl CancelHandle {
    /// 关闭连接；重复调用无副作用。
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

/// 建立 `CONNECT` 隧道，成功后同一 socket 可直接承载 TLS 会话。
fn tunnel_through_proxy<S: Read + Write>(
    socket: &mut S,
    host: &str,
    port: u16,
    authorization: Option<&str>,
    max_header_bytes: usize,
) -> Result<(), NetError> {
    let authority = format!("{host}:{port}");
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(value) = authorization {
        request.push_str(&format!("Proxy-Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    socket.write_all(request.as_bytes()).map_err(NetError::Body)?;
    socket.flush().map_err(NetError::Body)?;

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= max_header_bytes {
            return Err(NetError::HeaderTooLarge { limit: max_header_bytes });
        }
        socket.read_exact(&mut byte).map_err(NetError::Body)?;
        head.push(byte[0]);
    }
    let text = std::str::from_utf8(&head).map_err(|error| NetError::Transport(format!("proxy response is not UTF-8: {error}")))?;
    let status_line = text.split("\r\n").next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") || code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(NetError::Transport("invalid proxy CONNECT status line".into()));
    }
    let status = code.parse::<u16>().map_err(|_| NetError::Transport("invalid proxy CONNECT status code".into()))?;
    if !(200..300).contains(&status) {
        return Err(NetError::Transport(format!("proxy CONNECT to {authority} failed with status {status}")));
    }
    Ok(())
}

fn read_response<R: Read>(
    connection: &mut R,
    url: url::Url,
    method: &Method,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<Response, NetError> {
    let (status, headers) = read_response_head(connection, max_header_bytes)?;
    let body = match response_framing(method, status, &headers, max_body_bytes)? {
        Framing::Empty => Vec::new(),
        Framing::Length(length) => {
            let mut body = vec![0u8; length];
            connection.read_exact(&mut body).map_err(NetError::Body)?;
            body
        }
        Framing::Chunked(_) => read_chunked_body(connection, max_body_bytes, max_header_bytes)?,
        Framing::UntilClose => read_until_close(connection, max_body_bytes)?,
    };
    Ok(Response { url, status, headers, body, redirects: Vec::new() })
}

/// 读取最终响应（跳过 `1xx` 中间响应）的状态行与头块。
fn read_response_head<R: Read>(connection: &mut R, max_header_bytes: usize) -> Result<(u16, Headers), NetError> {
    let mut header_budget = max_header_bytes;
    let (status, headers) = loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= header_budget {
                return Err(NetError::HeaderTooLarge { limit: max_header_bytes });
            }
            connection.read_exact(&mut byte).map_err(NetError::Body)?;
            head.push(byte[0]);
        }
        let text = std::str::from_utf8(&head).map_err(|error| NetError::Transport(format!("response headers are not UTF-8: {error}")))?;
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let code = parts.next().unwrap_or("");
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(NetError::Transport("invalid HTTP status line".into()));
        }
        let status = code.parse::<u16>().map_err(|_| NetError::Transport("invalid HTTP status code".into()))?;
        if status < 100 || status == 101 {
            return Err(NetError::Unsupported("invalid status or protocol upgrade".into()));
        }
        let mut headers = Headers::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').ok_or_else(|| NetError::Transport("invalid response header".into()))?;
            validate_header(name, value)?;
            headers.append(name, value.trim().to_string());
        }
        header_budget -= head.len();
        if status >= 200 {
            break (status, headers);
        }
    };
    Ok((status, headers))
}

/// 由响应头决定正文 framing，并校验 `Content-Length` 与 `Transfer-Encoding`。
fn response_framing(method: &Method, status: u16, headers: &Headers, limit: usize) -> Result<Framing, NetError> {
    let lengths = headers.get_all("content-length");
    if lengths.len() > 1 {
        return Err(NetError::Transport("response has more than one Content-Length header".into()));
    }
    let length = lengths
        .first()
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(NetError::Transport("invalid Content-Length".into()));
            }
            value.parse::<usize>().map_err(|_| NetError::Transport("Content-Length overflows".into()))
        })
        .transpose()?;
    let transfer_encodings = headers.get_all("transfer-encoding");
    if !transfer_encodings.is_empty() && length.is_some() {
        return Err(NetError::Transport("conflicting Transfer-Encoding and Content-Length".into()));
    }
    if transfer_encodings.len() > 1 || transfer_encodings.first().is_some_and(|value| !value.eq_ignore_ascii_case("chunked")) {
        return Err(NetError::Unsupported("unsupported Transfer-Encoding".into()));
    }
    if *method == Method::Head || matches!(status, 204 | 304) {
        return Ok(Framing::Empty);
    }
    if !transfer_encodings.is_empty() {
        return Ok(Framing::Chunked(ChunkState { remaining: 0, needs_terminator: false }));
    }
    match length {
        Some(length) if length > limit => Err(NetError::BodyTooLarge { limit }),
        Some(length) => Ok(Framing::Length(length)),
        None => Ok(Framing::UntilClose),
    }
}

fn read_chunked_body<R: Read>(connection: &mut R, limit: usize, header_limit: usize) -> Result<Vec<u8>, NetError> {
    let mut output = Vec::new();
    let mut metadata_remaining = header_limit;
    loop {
        let line = read_line(connection, metadata_remaining.min(4096))?;
        metadata_remaining = metadata_remaining.saturating_sub(line.len() + 2);
        let size_text = line.split(';').next().unwrap_or("");
        if size_text.is_empty() || !size_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(NetError::Transport("invalid chunk size".into()));
        }
        let size = usize::from_str_radix(size_text, 16).map_err(|_| NetError::Transport("invalid chunk size".into()))?;
        if size == 0 {
            loop {
                let trailer = read_line(connection, metadata_remaining)?;
                metadata_remaining = metadata_remaining.saturating_sub(trailer.len() + 2);
                if trailer.is_empty() {
                    break;
                }
                let (name, value) = trailer.split_once(':').ok_or_else(|| NetError::Transport("invalid trailer".into()))?;
                validate_header(name, value)?;
                if matches!(name.to_ascii_lowercase().as_str(), "content-length" | "transfer-encoding" | "host") {
                    return Err(NetError::Transport("invalid framing trailer".into()));
                }
            }
            break;
        }
        if output.len().saturating_add(size) > limit {
            return Err(NetError::BodyTooLarge { limit });
        }
        let start = output.len();
        output.resize(start + size, 0);
        connection.read_exact(&mut output[start..]).map_err(NetError::Body)?;
        let mut crlf = [0u8; 2];
        connection.read_exact(&mut crlf).map_err(NetError::Body)?;
        if crlf != *b"\r\n" {
            return Err(NetError::Transport("invalid chunk terminator".into()));
        }
    }
    Ok(output)
}

fn read_until_close<R: Read>(connection: &mut R, limit: usize) -> Result<Vec<u8>, NetError> {
    let mut body = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = connection.read(&mut buffer).map_err(NetError::Body)?;
        if read == 0 {
            break;
        }
        if body.len().saturating_add(read) > limit {
            return Err(NetError::BodyTooLarge { limit });
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(body)
}

fn read_line<R: Read>(connection: &mut R, limit: usize) -> Result<String, NetError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if line.len() >= limit {
            return Err(NetError::HeaderTooLarge { limit });
        }
        connection.read_exact(&mut byte).map_err(NetError::Body)?;
        line.push(byte[0]);
    }
    String::from_utf8(line[..line.len() - 2].to_vec()).map_err(|error| NetError::Transport(error.to_string()))
}

fn validate_url(url: &url::Url) -> Result<(), NetError> {
    if url.host_str().is_none() || url.username() != "" || url.password().is_some() {
        return Err(NetError::Unsupported("URL must have a host and no credentials".into()));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NetError::Unsupported(format!("unsupported URL scheme `{}`", url.scheme())));
    }
    #[cfg(not(feature = "tls"))]
    if url.scheme() == "https" {
        return Err(NetError::Unsupported("https requires the `tls` feature".into()));
    }
    Ok(())
}

fn validate_redirect(from: &url::Url, to: &url::Url) -> Result<(), NetError> {
    validate_url(to)?;
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err(NetError::Unsupported("HTTPS downgrade redirect is not permitted".into()));
    }
    Ok(())
}

fn validate_header(name: &str, value: &str) -> Result<(), NetError> {
    if !is_token(name) || value.bytes().any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7f) {
        return Err(NetError::Unsupported("invalid header name or value".into()));
    }
    Ok(())
}

fn remaining_timeout(total: Duration, elapsed: Duration) -> Result<Duration, NetError> {
    total.checked_sub(elapsed).filter(|value| !value.is_zero()).ok_or_else(|| NetError::Transport("whole-request timeout exceeded".into()))
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}
fn should_switch_to_get(status: u16, method: &Method) -> bool {
    (status == 303 && *method != Method::Head) || (matches!(status, 301 | 302) && *method == Method::Post)
}
fn remove_sensitive_headers(headers: &mut Headers) {
    for name in ["authorization", "proxy-authorization", "cookie", "host"] {
        headers.remove(name);
    }
}
fn remove_entity_headers(headers: &mut Headers) {
    for name in ["content-length", "content-type", "transfer-encoding"] {
        headers.remove(name);
    }
}
