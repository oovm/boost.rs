//! 把云调用结果翻译成可执行的根因结论。
//!
//! 核心只有一件事：区分「确定没有发出」「明确被拒」「可能已经生效」。第三类绝不给出
//! 可自动重试的结论，因为短信没有幂等键、对象存储的写入也可能已经落地。

use atlas_cloud::tencent::{CloudError, CloudOutcome, RequestId, cos::CosOutcome, sms::SmsOutcome};

/// 明确属于参数、鉴权或额度类的厂商错误码前缀。
///
/// 这类错误在服务端通常发生在业务动作之前，修正配置或配额后重试不会产生重复副作用。
const RETRY_SAFE_CODE_PREFIXES: &[&str] = &[
    "AuthFailure",
    "InvalidParameter",
    "UnauthorizedOperation",
    "LimitExceeded",
    "RequestLimitExceeded",
    "FailedOperation.Insufficient",
    "FailedOperation.Quota",
];

/// 判定「不确定」结果来源的关键词；命中传输层词汇才算传输问题。
const TRANSPORT_HINTS: &[&str] = &["传输层", "连接", "超时", "拨号", "tls", "dns", "network", "timeout", "connection", "reset"];

/// COS `PUT` 的幂等说明。
///
/// `diagnose_cos` 只拿到结果、拿不到请求方法，因此结论必须写成条件句，不能替调用方假设
/// 本次一定是 `PUT`。
const PUT_REPLAY_NOTE: &str = "若本次是 PUT：同 key 同内容重放安全（覆盖写幂等）；GET/HEAD 只读，其他操作不要套用这个结论";

/// 出错或结果落地所在的层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// 本地配置：凭证、应用 ID、地域等缺失或为空，请求没有发出。
    Config,
    /// 传输层：本地构造失败（确定没有发出），或连接、超时、读响应中断（可能已生效）。
    Transport,
    /// 协议/合同层：已收到响应但结构不符合合同，无法判定请求是否生效。
    Protocol,
    /// 厂商业务层：服务端明确接受或拒绝，附带厂商错误码。
    Vendor,
    /// 计费层：保留给计费对账，当前腾讯云服务不在该层产生诊断。
    Billing,
}

/// 一次诊断的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    /// 问题所在的层。
    pub layer: Layer,
    /// 一句话结论，包含错误码与关键证据。
    pub summary: String,
    /// `true` 表示重试不会造成重复副作用；`false` 表示可能已生效，禁止自动重试。
    pub retry_safe: bool,
    /// 厂商请求 ID，对账时使用。
    pub request_id: Option<String>,
    /// 可执行的下一步排查动作。
    pub next_steps: Vec<String>,
}

/// 诊断一个 [`CloudError`]。
///
/// [`CloudError::Indeterminate`] 按原因文本判断来自传输层还是协议层，但两种情况都返回
/// `retry_safe: false`。
pub fn diagnose_error(error: &CloudError) -> Diagnosis {
    match error {
        CloudError::InvalidConfig(key) => Diagnosis {
            layer: Layer::Config,
            summary: format!("本地配置缺失：`{key}` 未设置或为空白，请求没有发出"),
            retry_safe: true,
            request_id: None,
            next_steps: vec![
                format!("设置环境变量 `{key}`，空字符串按缺失处理"),
                "确认部署单元与 CI 已注入该变量，不要依赖本地配置文件".to_string(),
                "在进程启动时用 `Credentials::from_env()` 自检，失败立即退出".to_string(),
            ],
        },
        CloudError::Rejected { status, code, message, request_id } => {
            let retry_safe = vendor_retry_safe(code);
            let mut next_steps = vec![request_step(request_id.as_ref()), format!("对照官方错误码文档确认 `{code}` 的处置口径")];
            next_steps.push(vendor_retry_note(code, retry_safe));
            Diagnosis {
                layer: Layer::Vendor,
                summary: format!("服务端明确拒绝（HTTP {status}，{code}）：{message}"),
                retry_safe,
                request_id: request_id_text(request_id.as_ref()),
                next_steps,
            }
        }
        CloudError::Indeterminate { reason, request_id } => indeterminate(reason, request_id.as_ref()),
        CloudError::Transport(detail) => Diagnosis {
            layer: Layer::Transport,
            summary: format!("请求在本地被拒绝，确定没有发出：{detail}"),
            retry_safe: true,
            request_id: None,
            next_steps: vec!["修正非法 URL、非法头或补齐 TLS 能力后重试".to_string(), "本层确定没有计费或写入，不需要向厂商对账".to_string()],
        },
        CloudError::Decode(detail) => Diagnosis {
            layer: Layer::Protocol,
            summary: format!("已收到响应但不符合合同：{detail}"),
            retry_safe: false,
            request_id: None,
            next_steps: vec![
                "不要当作成功：短信可能已经下发，对象可能已经写入".to_string(),
                "用 RequestId 向厂商对账（本次响应未回传，按调用时间窗口与业务键检索）".to_string(),
                "核对响应体字段是否被网关、代理或重定向改写".to_string(),
            ],
        },
    }
}

/// 诊断一条短信结果。
///
/// 成功下发（`Sent`）属于厂商层而不是计费层：结论是「已生效、不要重发」，而不是继续重试。
pub fn diagnose_sms(outcome: &SmsOutcome) -> Diagnosis {
    match outcome {
        SmsOutcome::Sent { serial_no, request_id } => Diagnosis {
            layer: Layer::Vendor,
            summary: format!("短信已受理下发（SerialNo={serial_no}，RequestId={request_id}）"),
            retry_safe: false,
            request_id: Some(request_id.as_str().to_string()),
            next_steps: vec![
                "不要重发：SendSms 没有幂等键，重发会重复下发并重复计费".to_string(),
                format!("用 SerialNo `{serial_no}` 与 RequestId 向厂商对账，核对下发状态"),
                "如需补发，先确认号码是否已收到，再发起新请求".to_string(),
            ],
        },
        SmsOutcome::Rejected { code, message, request_id } => {
            let retry_safe = vendor_retry_safe(code);
            let mut next_steps = vec![request_step(request_id.as_ref()), format!("对照官方错误码文档确认 `{code}` 的处置口径")];
            next_steps.push(vendor_retry_note(code, retry_safe));
            Diagnosis {
                layer: Layer::Vendor,
                summary: format!("短信服务明确拒绝（{code}）：{message}"),
                retry_safe,
                request_id: request_id_text(request_id.as_ref()),
                next_steps,
            }
        }
        SmsOutcome::Indeterminate { reason, request_id } => {
            let mut diagnosis = indeterminate(reason, request_id.as_ref());
            diagnosis.next_steps.push("短信没有幂等键：先确认号码是否已收到，再决定是否人工补发".to_string());
            diagnosis
        }
    }
}

/// 诊断一次 COS 结果。
///
/// COS 的 `PUT` 同 key 覆盖写天然幂等，因此成功与「不确定」都会附带
/// 「同 key 同内容重放安全」的说明；但结果类型不携带请求方法，`GET` / `HEAD` 之外的
/// 结论不能泛化。
pub fn diagnose_cos(outcome: &CosOutcome) -> Diagnosis {
    match outcome {
        CloudOutcome::Succeeded { value, request_id } => Diagnosis {
            layer: Layer::Vendor,
            summary: format!("COS 请求已成功（RequestId={request_id}，返回 {} 字节）", value.body.len()),
            retry_safe: false,
            request_id: Some(request_id.as_str().to_string()),
            next_steps: vec![
                "已经生效，不要自动重发".to_string(),
                PUT_REPLAY_NOTE.to_string(),
                format!("用 RequestId `{request_id}` 在 COS 日志与控制台核对"),
            ],
        },
        CloudOutcome::Rejected { code, message, request_id } => {
            let retry_safe = vendor_retry_safe(code);
            let mut next_steps = vec![request_step(request_id.as_ref()), format!("对照官方错误码文档确认 `{code}` 的处置口径")];
            next_steps.push(vendor_retry_note(code, retry_safe));
            next_steps.push(PUT_REPLAY_NOTE.to_string());
            Diagnosis {
                layer: Layer::Vendor,
                summary: format!("COS 明确拒绝（{code}）：{message}"),
                retry_safe,
                request_id: request_id_text(request_id.as_ref()),
                next_steps,
            }
        }
        CloudOutcome::Indeterminate { reason, request_id } => {
            let mut diagnosis = indeterminate(reason, request_id.as_ref());
            diagnosis.next_steps.push(PUT_REPLAY_NOTE.to_string());
            diagnosis
        }
    }
}

/// 「结果不确定」的统一结论：永远不允许自动重试。
fn indeterminate(reason: &str, request_id: Option<&RequestId>) -> Diagnosis {
    let layer = source_layer(reason);
    let next_steps = vec![
        "不要自动重试：结果不确定，重试可能重复下发、重复写入或重复计费".to_string(),
        request_step(request_id),
        match layer {
            Layer::Transport => "检查网络、代理、TLS 与超时设置；修复后仍要先对账再重试".to_string(),
            _ => "核对响应是否被网关改写或截断，再决定下一步".to_string(),
        },
    ];
    Diagnosis {
        layer,
        summary: format!("结果不确定，可能已经生效：{reason}"),
        retry_safe: false,
        request_id: request_id_text(request_id),
        next_steps,
    }
}

/// 判断「不确定」来自传输层还是协议层。
fn source_layer(reason: &str) -> Layer {
    let lowered = reason.to_ascii_lowercase();
    if TRANSPORT_HINTS.iter().any(|hint| lowered.contains(hint)) { Layer::Transport } else { Layer::Protocol }
}

/// 厂商错误码是否属于参数、鉴权或额度类。
fn vendor_retry_safe(code: &str) -> bool {
    if let Some(status) = code.strip_prefix("HTTP_").and_then(|text| text.parse::<u16>().ok()) {
        return matches!(status, 400 | 401 | 403 | 404 | 429);
    }
    RETRY_SAFE_CODE_PREFIXES.iter().any(|prefix| code.starts_with(prefix))
}

/// 重试安全性的一句话说明。
fn vendor_retry_note(code: &str, retry_safe: bool) -> String {
    if retry_safe {
        format!("`{code}` 属于参数、鉴权或额度类错误：修正后重试是安全的")
    }
    else {
        format!("`{code}` 不属于参数、鉴权或额度类错误：不要按同一请求直接重试，先确认服务端状态")
    }
}

/// 对账动作；没有请求 ID 时也要给出可执行的检索路径。
fn request_step(request_id: Option<&RequestId>) -> String {
    match request_id {
        Some(request_id) => format!("用 RequestId 向厂商对账：`{request_id}`，确认请求是否已生效"),
        None => "用 RequestId 向厂商对账（本次响应未回传，按调用时间窗口与业务键检索）".to_string(),
    }
}

/// 把可选请求 ID 投影成报告用的字符串。
fn request_id_text(request_id: Option<&RequestId>) -> Option<String> {
    request_id.map(|request_id| request_id.as_str().to_string())
}
