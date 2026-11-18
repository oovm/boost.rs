//! 腾讯云 COS（对象存储）：`q-sign-algorithm=sha1` 签名与 `PUT` / `GET` / `HEAD` Object。
//!
//! **签名按公开公式实现，尚未与官方示例向量对齐，真实调用前必须补齐。** 本模块只保证
//! 公式自洽（同一输入稳定、改路径即改签名）与结构正确（`KeyTime` 为 Unix 秒对、
//! `q-header-list` 小写有序），不声称与腾讯云官方示例一致。
//!
//! 签名只覆盖 `host` 一个头，因此 `q-header-list=host`、`q-url-param-list` 恒为空；对象键经
//! 百分号编码后同时用于请求行与签名，两者不会漂移。COS 的错误响应体是 XML，本批不解析，
//! 非 2xx 的 `code` 由 HTTP 状态派生。
//!
//! [`DEFAULT_KEY_TIME_SECONDS`] 是默认的签名有效期窗口；单次调用不重试。
//! `PUT Object` 同 key 覆盖写、天然幂等，`GET` / `HEAD` 只读，是否重试由调用方按
//! [`CosOutcome::Indeterminate`] 的含义决定。

use std::fmt::{Display, Formatter};

use boost_crypto::{hash::sha1, mac::hmac_sha1};
use boost_net::{
    Url,
    http::{Headers, HttpTransport, Method, Request, Transport},
};

use super::{CloudError, CloudOutcome, Credentials, Region, RequestId, body_snippet, map_transport_error, now_seconds, to_hex};

/// 默认的签名有效期窗口（秒）。
pub const DEFAULT_KEY_TIME_SECONDS: u64 = 300;

/// 百分号编码使用的大写十六进制字符表。
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

/// COS 对象的描述。
///
/// `PUT` / `HEAD` 没有响应体，`body` 为空；`GET` 返回对象内容。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CosObject {
    /// 响应体；`PUT` / `HEAD` 恒为空。
    pub body: Vec<u8>,
    /// `Content-Type` 头。
    pub content_type: Option<String>,
    /// `Content-Length` 头。
    pub content_length: Option<u64>,
    /// `ETag` 头，已去掉外层引号。
    pub etag: Option<String>,
}

/// COS 单次调用结果。
pub type CosOutcome = CloudOutcome<CosObject>;

/// `q-key-time` / `q-sign-time` 的有效期：一对 Unix 秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTime {
    start: u64,
    end: u64,
}

impl KeyTime {
    /// 用显式的起止 Unix 秒构造。
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// 从当前时间开始、持续 `seconds` 秒的窗口。
    pub fn window(seconds: u64) -> Self {
        let start = now_seconds();
        Self { start, end: start.saturating_add(seconds) }
    }
}

impl Display for KeyTime {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let Self { start, end } = *self;
        write!(formatter, "{start};{end}")
    }
}

/// COS 客户端。
///
/// 桶名按调用传入（不含 APPID），APPID、地域与凭证在构造时固定；访问域名形如
/// `<bucket>-<app_id>.cos.<region>.myqcloud.com`。
pub struct Client {
    credentials: Credentials,
    region: Region,
    app_id: String,
    transport: HttpTransport,
}

impl Client {
    /// 用凭证、地域与 APPID 构造客户端。
    pub fn new(credentials: Credentials, region: Region, app_id: impl Into<String>) -> Self {
        Self { credentials, region, app_id: app_id.into(), transport: HttpTransport::new() }
    }

    /// 计算 `q-signature` 的十六进制值。
    ///
    /// 纯函数：只依赖 `bucket`、`method`、`uri_path`（必须是已百分号编码、与请求行一致的
    /// 路径）与 `key_time`，不读时钟、不联网。
    pub fn signature(&self, bucket: &str, method: &str, uri_path: &str, key_time: &KeyTime) -> String {
        let key_time = key_time.to_string();
        let sign_key = to_hex(&hmac(self.credentials.secret_key().as_bytes(), key_time.as_bytes()));
        let http_string = format!("{}\n{uri_path}\n\nhost={}\n", method.to_ascii_lowercase(), self.host(bucket));
        let string_to_sign = format!("sha1\n{key_time}\n{}\n", to_hex(&sha1(http_string.as_bytes())));
        to_hex(&hmac(sign_key.as_bytes(), string_to_sign.as_bytes()))
    }

    /// 组装完整的 `Authorization` 头。
    pub fn authorization(&self, bucket: &str, method: &str, uri_path: &str, key_time: &KeyTime) -> String {
        let signature = self.signature(bucket, method, uri_path, key_time);
        let key_time = key_time.to_string();
        format!(
            "q-sign-algorithm=sha1&q-ak={}&q-sign-time={key_time}&q-key-time={key_time}&q-header-list=host&q-url-param-list=&q-signature={signature}",
            self.credentials.secret_id()
        )
    }

    /// 上传（覆盖写）一个对象。
    ///
    /// 同 key 覆盖写，服务端幂等；传输层失败仍返回 `Indeterminate`，重试由调用方决定。
    pub fn put_object(&self, bucket: &str, key: &str, body: &[u8]) -> Result<CosOutcome, CloudError> {
        self.execute(bucket, Method::Put, key, body)
    }

    /// 读取一个对象的内容。
    pub fn get_object(&self, bucket: &str, key: &str) -> Result<CosOutcome, CloudError> {
        self.execute(bucket, Method::Get, key, &[])
    }

    /// 读取对象元数据，不下载内容。
    pub fn head_object(&self, bucket: &str, key: &str) -> Result<CosOutcome, CloudError> {
        self.execute(bucket, Method::Head, key, &[])
    }

    fn execute(&self, bucket: &str, method: Method, key: &str, body: &[u8]) -> Result<CosOutcome, CloudError> {
        let uri_path = object_path(key);
        let key_time = KeyTime::window(DEFAULT_KEY_TIME_SECONDS);
        let authorization = self.authorization(bucket, method.as_str(), &uri_path, &key_time);
        let url = Url::parse(&format!("https://{}{uri_path}", self.host(bucket))).map_err(|error| CloudError::Transport(error.to_string()))?;
        let request = Request::new(method, url).with_header("authorization", authorization).with_body(body.to_vec());
        let response = self.transport.send(&request).map_err(map_transport_error)?;
        interpret_response(response.status, &response.headers, &response.body)
    }

    /// 访问域名。
    fn host(&self, bucket: &str) -> String {
        let app_id = &self.app_id;
        let region = self.region.as_str();
        format!("{bucket}-{app_id}.cos.{region}.myqcloud.com")
    }
}

/// 把一次 HTTP 响应映射为 COS 结果。
///
/// COS 用 `x-cos-request-id` 头而不是响应体回传请求 ID；非 2xx 一律是服务端明确拒绝，
/// `code` 由状态派生为 `HTTP_<status>`，`message` 取错误 XML 的前 256 个字符。2xx 但缺少
/// 请求 ID 时返回 [`CloudError::Decode`]，不伪造成功。
pub fn interpret_response(status: u16, headers: &Headers, body: &[u8]) -> Result<CosOutcome, CloudError> {
    let request_id = headers.get("x-cos-request-id").filter(|value| !value.is_empty()).map(RequestId::new);

    if !(200..300).contains(&status) {
        return Ok(CloudOutcome::Rejected { code: format!("HTTP_{status}"), message: body_snippet(body), request_id });
    }

    let Some(request_id) = request_id
    else {
        return Err(CloudError::Decode("COS 响应缺少 x-cos-request-id，无法归属".to_string()));
    };
    let object = CosObject {
        body: body.to_vec(),
        content_type: headers.get("content-type").map(ToString::to_string),
        content_length: headers.get("content-length").and_then(|value| value.parse::<u64>().ok()),
        etag: headers.get("etag").map(|value| value.trim_matches('"').to_string()),
    };
    Ok(CloudOutcome::Succeeded { value: object, request_id })
}

/// 对象键 → 请求行里的 URI 路径。
///
/// 保留 `/` 作为层级分隔，其余字节按 RFC 3986 的 unreserved 集合之外的字符百分号编码；
/// 前导 `/` 会被去掉，保证路径只有一个分隔符开头。
fn object_path(key: &str) -> String {
    let mut path = String::with_capacity(key.len() + 1);
    path.push('/');
    for byte in key.trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => path.push(byte as char),
            _ => {
                path.push('%');
                path.push(UPPER_HEX[(byte >> 4) as usize] as char);
                path.push(UPPER_HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    path
}

/// HMAC-SHA1 包装：HMAC 对任意长度密钥都有定义，失败分支不可达。
fn hmac(key: &[u8], data: &[u8]) -> [u8; 20] {
    hmac_sha1(key, data).expect("HMAC 接受任意长度密钥，不会失败")
}
