//! 腾讯云首批服务的共享作者面。
//!
//! 只承载两家服务都要用的词汇：凭证 [`Credentials`]、地域 [`Region`]、厂商请求 ID
//! [`RequestId`]、分层错误 [`CloudError`] 与结果三态 [`CloudOutcome`]。服务语义留在
//! [`sms`] 与 [`cos`]。

use std::{
    fmt::{Debug, Display, Formatter},
    time::{SystemTime, UNIX_EPOCH},
};

pub mod cos;
pub mod sms;

pub(crate) use crate::error::{body_snippet, map_transport_error};

/// SecretId 的环境变量名。
const SECRET_ID_ENV: &str = "TENCENTCLOUD_SECRET_ID";
/// SecretKey 的环境变量名。
const SECRET_KEY_ENV: &str = "TENCENTCLOUD_SECRET_KEY";

/// 腾讯云 API 凭证。
///
/// 字段私有，只能经 [`Credentials::new`] 或 [`Credentials::from_env`] 注入；手写的
/// [`Debug`] 把两段都替换成 `"[redacted]"`，凭证不会随日志或 panic 信息落盘。
#[derive(Clone)]
pub struct Credentials {
    secret_id: String,
    secret_key: String,
}

impl Credentials {
    /// 用显式凭证构造。
    pub fn new(secret_id: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self { secret_id: secret_id.into(), secret_key: secret_key.into() }
    }

    /// 从 `TENCENTCLOUD_SECRET_ID` 与 `TENCENTCLOUD_SECRET_KEY` 读取凭证。
    ///
    /// 任一变量缺失或为空返回 [`CloudError::InvalidConfig`]，参数是缺失的变量名。
    /// 不猜默认值，也不读配置文件。
    pub fn from_env() -> Result<Self, CloudError> {
        Ok(Self { secret_id: read_env(SECRET_ID_ENV)?, secret_key: read_env(SECRET_KEY_ENV)? })
    }

    /// 返回 SecretId。
    ///
    /// SecretId 会原样出现在签名头里（`Credential` / `q-ak`），不按秘密材料处理。
    pub fn secret_id(&self) -> &str {
        &self.secret_id
    }

    /// 返回 SecretKey，只供本 crate 内部的签名计算使用。
    pub(crate) fn secret_key(&self) -> &str {
        &self.secret_key
    }
}

impl Debug for Credentials {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Credentials").field("secret_id", &"[redacted]").field("secret_key", &"[redacted]").finish()
    }
}

/// 腾讯云地域，例如 `ap-guangzhou`。
///
/// 只做字符串包装，不枚举地域：新地域无需改本 crate 即可使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region(String);

impl Region {
    /// 用地域名构造。
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// 广州地域。
    pub fn ap_guangzhou() -> Self {
        Self::new("ap-guangzhou")
    }

    /// 返回地域名。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Region {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 厂商请求 ID。
///
/// 腾讯云在每个响应里回传 `RequestId`（COS 走 `x-cos-request-id` 头），它是排障与对账的
/// 唯一凭据，因此成功、明确拒绝与不确定三种结果都尽量保留它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(String);

impl RequestId {
    /// 用厂商返回值构造。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 返回原始字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RequestId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 云调用结果三态：成功、明确拒绝、结果不确定。
///
/// 单个服务可以按自己的成功语义另立枚举（例如 SMS 的 `Sent`）；COS 直接复用本类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudOutcome<T> {
    /// 服务端确认成功。
    Succeeded {
        /// 结构化成功值。
        value: T,
        /// 厂商请求 ID。
        request_id: RequestId,
    },
    /// 服务端明确拒绝。
    Rejected {
        /// 厂商错误码；无法解析厂商错误体时由 HTTP 状态派生为 `HTTP_<status>`。
        code: String,
        /// 厂商错误描述或响应体片段。
        message: String,
        /// 厂商请求 ID；网关类错误可能没有。
        request_id: Option<RequestId>,
    },
    /// 无法判定请求是否已经生效。
    Indeterminate {
        /// 无法判定的原因。
        reason: String,
        /// 厂商请求 ID；传输阶段失败时通常没有。
        request_id: Option<RequestId>,
    },
}

impl<T> CloudOutcome<T> {
    /// 返回本次调用的厂商请求 ID（成功时一定存在）。
    pub fn request_id(&self) -> Option<&RequestId> {
        match self {
            Self::Succeeded { request_id, .. } => Some(request_id),
            Self::Rejected { request_id, .. } | Self::Indeterminate { request_id, .. } => request_id.as_ref(),
        }
    }

    /// 返回服务端是否确认成功。
    pub fn is_succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }
}

/// 云调用错误。
///
/// 分层的意义在于调用方能区分「没发出去」「被拒了」「不知道生效没有」，三者对应的处置
/// 完全不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudError {
    /// 本地配置缺失或为空；参数是缺失的配置键名。请求没有发出。
    InvalidConfig(String),
    /// 服务端明确拒绝，但响应无法用结构化结果表达（例如网关返回非厂商格式）。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 厂商错误码；无法解析时由状态派生为 `HTTP_<status>`。
        code: String,
        /// 厂商错误描述或响应体片段。
        message: String,
        /// 厂商请求 ID；网关类错误可能没有。
        request_id: Option<RequestId>,
    },
    /// 无法判定请求是否已经生效：可能已经发送、计费或写入。
    ///
    /// 调用方不得把它折叠成普通失败，也不得自动重试。
    Indeterminate {
        /// 无法判定的原因。
        reason: String,
        /// 厂商请求 ID；传输阶段失败时通常没有。
        request_id: Option<RequestId>,
    },
    /// 请求在本地就被拒绝，确定没有发出（非法 URL、非法头、缺少 TLS 能力等）。
    Transport(String),
    /// 已收到响应但不符合合同，例如成功响应缺少关键字段。
    ///
    /// 是否生效由具体字段决定，调用方按最保守的假设处理。
    Decode(String),
}

impl Display for CloudError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(key) => write!(formatter, "缺少或为空的配置 `{key}`"),
            Self::Rejected { status, code, message, request_id } => {
                write!(formatter, "服务端拒绝（HTTP {status}，{code}）：{message}")?;
                match request_id {
                    Some(request_id) => write!(formatter, "（RequestId={request_id}）"),
                    None => Ok(()),
                }
            }
            Self::Indeterminate { reason, request_id } => {
                write!(formatter, "结果不确定，可能已经生效：{reason}")?;
                match request_id {
                    Some(request_id) => write!(formatter, "（RequestId={request_id}）"),
                    None => Ok(()),
                }
            }
            Self::Transport(detail) => write!(formatter, "请求未能发出：{detail}"),
            Self::Decode(detail) => write!(formatter, "响应不符合合同：{detail}"),
        }
    }
}

impl std::error::Error for CloudError {}

/// 读取环境变量，缺失或全空白按缺失处理。
fn read_env(name: &'static str) -> Result<String, CloudError> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty()).ok_or_else(|| CloudError::InvalidConfig(name.to_string()))
}

/// 当前的 Unix 秒。
pub(crate) fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or_default()
}
