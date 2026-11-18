//! 腾讯云短信 `SendSms`（Version `2021-01-11`）与 TC3-HMAC-SHA256 签名。
//!
//! 签名串按腾讯云公开的 TC3 规范组装：规范请求（`POST` + `/` + 空查询串 + 小写头 + 签名头
//! 列表 `content-type;host`）、凭证范围 `<date>/sms/tc3_request`、四层 HMAC-SHA256 派生密钥。
//! [`Client::authorization`] 是纯函数，固定 payload 与时间戳即可在不联网的情况下单测。
//!
//! 响应处理见 [`interpret_response`]：能结构化表达的明确拒绝进 [`SmsOutcome::Rejected`]，
//! 只有连厂商格式都不成立的错误响应才走 [`CloudError::Rejected`]。

use boost_crypto::{encoding::hex_encode, hash::sha256, mac::hmac_sha256};
use boost_net::{
    Url,
    http::{HttpTransport, Method, Request, Transport},
};
use serde_json::{Value, json};

use super::{CloudError, Credentials, Region, RequestId, body_snippet, map_transport_error, now_seconds};

/// 短信服务端点。
const HOST: &str = "sms.tencentcloudapi.com";
/// TC3 凭证范围里的服务名。
const SERVICE: &str = "sms";
/// 接口名。
const ACTION: &str = "SendSms";
/// 接口版本。
const VERSION: &str = "2021-01-11";
/// 请求体类型，参与签名。
const CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// 短信模板。
///
/// 模板 ID 与参数集由腾讯云控制台定义，本 crate 只负责按顺序填入
/// `TemplateParamSet`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// 模板 ID。
    pub id: String,
    /// 模板参数，按位置对应模板占位符。
    pub params: Vec<String>,
}

impl Template {
    /// 用模板 ID 构造空参数模板。
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), params: Vec::new() }
    }

    /// 追加一个模板参数。
    pub fn param(mut self, value: impl Into<String>) -> Self {
        self.params.push(value.into());
        self
    }
}

/// 一条待发送的短信。
///
/// 手机号按腾讯云 `E.164` 口径（如 `+8613800138000`）原样提交，本 crate 不做号码归一化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendSms {
    /// 接收号码列表。
    pub phones: Vec<String>,
    /// 已审核通过的签名内容。
    pub sign_name: String,
    /// 模板与参数。
    pub template: Template,
}

/// 单次 `SendSms` 的结果。
///
/// `SendSms` 没有幂等键，`Indeterminate` 意味着短信可能已经下发并计费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsOutcome {
    /// 服务端受理成功。
    Sent {
        /// 下发流水号 `SerialNo`。
        serial_no: String,
        /// 厂商请求 ID。
        request_id: RequestId,
    },
    /// 服务端明确拒绝（HTTP 层拒绝或业务码非 `Ok`）。
    Rejected {
        /// 厂商错误码。
        code: String,
        /// 厂商错误描述。
        message: String,
        /// 厂商请求 ID；网关类错误可能没有。
        request_id: Option<RequestId>,
    },
    /// 请求已送达但结果无法判定，调用方不得自动重试。
    Indeterminate {
        /// 无法判定的原因。
        reason: String,
        /// 厂商请求 ID；传输阶段失败时通常没有。
        request_id: Option<RequestId>,
    },
}

/// 短信客户端。
///
/// 默认单次发送，不内建重试。凭证、`SdkAppId` 与地域在构造时固定。
pub struct Client {
    credentials: Credentials,
    sdk_app_id: String,
    region: Region,
    transport: Box<dyn Transport + Send + Sync>,
}

impl Client {
    /// 用凭证、短信应用 `SdkAppId` 与地域构造客户端。
    pub fn new(credentials: Credentials, sdk_app_id: impl Into<String>, region: Region) -> Self {
        Self { credentials, sdk_app_id: sdk_app_id.into(), region, transport: Box::new(HttpTransport::new()) }
    }

    /// 替换底层传输。
    ///
    /// 供测试替身、受控回放或自建代理使用；默认是 [`HttpTransport`]。
    pub fn with_transport(mut self, transport: Box<dyn Transport + Send + Sync>) -> Self {
        self.transport = transport;
        self
    }

    /// 计算完整的 `Authorization` 头。
    ///
    /// 纯函数：同一 `payload` 与 `timestamp` 永远得到同一结果，不读时钟、不联网。
    pub fn authorization(&self, payload: &str, timestamp: u64) -> String {
        let date = utc_date(timestamp);
        let canonical =
            format!("POST\n/\n\ncontent-type:{CONTENT_TYPE}\nhost:{HOST}\n\ncontent-type;host\n{}", hex_encode(&sha256(payload.as_bytes())));
        let scope = format!("{date}/{SERVICE}/tc3_request");
        let string_to_sign = format!("TC3-HMAC-SHA256\n{timestamp}\n{scope}\n{}", hex_encode(&sha256(canonical.as_bytes())));

        let secret_key = self.credentials.secret_key();
        let secret_date = hmac(format!("TC3{secret_key}").as_bytes(), date.as_bytes());
        let secret_service = hmac(&secret_date, SERVICE.as_bytes());
        let secret_signing = hmac(&secret_service, b"tc3_request");
        let signature = hex_encode(&hmac(&secret_signing, string_to_sign.as_bytes()));

        format!("TC3-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=content-type;host, Signature={signature}", self.credentials.secret_id())
    }

    /// 发送一条短信，单次调用，不重试。
    pub fn send(&self, message: &SendSms) -> Result<SmsOutcome, CloudError> {
        let payload = self.payload(message);
        let timestamp = now_seconds();
        let authorization = self.authorization(&payload, timestamp);
        let url = Url::parse(&format!("https://{HOST}")).map_err(|error| CloudError::Transport(error.to_string()))?;
        let request = Request::new(Method::Post, url)
            .with_header("content-type", CONTENT_TYPE)
            .with_header("x-tc-action", ACTION)
            .with_header("x-tc-version", VERSION)
            .with_header("x-tc-timestamp", timestamp.to_string())
            .with_header("x-tc-region", self.region.as_str())
            .with_header("authorization", authorization)
            .with_body(payload.into_bytes());

        let response = self.transport.send(&request).map_err(map_transport_error)?;
        interpret_response(response.status, &response.body)
    }

    /// 构造请求体。
    ///
    /// 只序列化一次，签名与发送共用同一份字节，避免序列化差异让签名失效。
    fn payload(&self, message: &SendSms) -> String {
        json!({
            "PhoneNumberSet": &message.phones,
            "SignName": &message.sign_name,
            "SmsSdkAppId": &self.sdk_app_id,
            "TemplateId": &message.template.id,
            "TemplateParamSet": &message.template.params,
        })
        .to_string()
    }
}

/// 把一次 HTTP 响应映射为短信结果。
///
/// 传输入口只把「拿到响应」的情况交到这里；传输失败直接归入
/// [`CloudError::Indeterminate`]，两者不能互相折叠。
///
/// 分层规则：
///
/// - 非 2xx 且响应是腾讯云错误结构 → [`SmsOutcome::Rejected`]，带 `RequestId`。
/// - 非 2xx 且响应不是腾讯云结构（网关页等）→ [`CloudError::Rejected`]。
/// - 2xx 但响应不是腾讯云结构 → [`SmsOutcome::Indeterminate`]，请求确实送达过。
/// - 2xx 且业务码非 `Ok` → [`SmsOutcome::Rejected`]。
/// - 2xx 且业务码为 `Ok`：必须同时具备 `SerialNo` 与 `RequestId`，否则
///   [`CloudError::Decode`]，绝不伪造成成功。
pub fn interpret_response(status: u16, body: &[u8]) -> Result<SmsOutcome, CloudError> {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let response = parsed.as_ref().and_then(|value| value.get("Response")).filter(|value| value.is_object());

    if !(200..300).contains(&status) {
        let request_id = response.and_then(request_id_of);
        let failure = response.and_then(|value| value.get("Error"));
        let code = failure.and_then(|error| error.get("Code")).and_then(Value::as_str).unwrap_or_default();
        if !code.is_empty() {
            let message = failure.and_then(|error| error.get("Message")).and_then(Value::as_str).unwrap_or_default();
            return Ok(SmsOutcome::Rejected { code: code.to_string(), message: message.to_string(), request_id });
        }
        return Err(CloudError::Rejected { status, code: format!("HTTP_{status}"), message: body_snippet(body), request_id });
    }

    let Some(response) = response
    else {
        return Ok(SmsOutcome::Indeterminate {
            reason: "HTTP 2xx 但响应体不是腾讯云 API 响应，无法判定是否已发送".to_string(),
            request_id: None,
        });
    };
    let request_id = request_id_of(response);

    if let Some(error) = response.get("Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or_default();
        let message = error.get("Message").and_then(Value::as_str).unwrap_or_default();
        return Ok(SmsOutcome::Rejected { code: code.to_string(), message: message.to_string(), request_id });
    }

    let entry = response
        .get("SendStatusSet")
        .and_then(|value| value.get(0))
        .ok_or_else(|| CloudError::Decode("响应缺少 SendStatusSet[0]".to_string()))?;
    let Some(code) = entry.get("Code").and_then(Value::as_str).filter(|value| !value.is_empty())
    else {
        return Err(CloudError::Decode("响应缺少 SendStatusSet[0].Code".to_string()));
    };
    if code != "Ok" {
        let message = entry.get("Message").and_then(Value::as_str).unwrap_or_default();
        return Ok(SmsOutcome::Rejected { code: code.to_string(), message: message.to_string(), request_id });
    }

    let Some(serial_no) = entry.get("SerialNo").and_then(Value::as_str).filter(|value| !value.is_empty())
    else {
        return Err(CloudError::Decode("下发成功但响应缺少 SerialNo".to_string()));
    };
    let Some(request_id) = request_id
    else {
        return Err(CloudError::Decode("下发成功但响应缺少 RequestId，无法归属".to_string()));
    };
    Ok(SmsOutcome::Sent { serial_no: serial_no.to_string(), request_id })
}

/// 取响应里的 `RequestId`，空串按缺失处理。
fn request_id_of(response: &Value) -> Option<RequestId> {
    response.get("RequestId").and_then(Value::as_str).filter(|value| !value.is_empty()).map(RequestId::new)
}

/// HMAC-SHA256 包装：HMAC 对任意长度密钥都有定义，失败分支不可达。
fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    hmac_sha256(key, data).expect("HMAC 接受任意长度密钥，不会失败")
}

/// Unix 秒 → UTC `YYYY-MM-DD`。
///
/// 用 Howard Hinnant 的 `civil_from_days` 换算，不引日历依赖，也不受本地时区影响。
fn utc_date(timestamp: u64) -> String {
    let days = (timestamp / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
