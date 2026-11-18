//! 稳定的报告投影与敏感值回归检查。
//!
//! [`Record`] 只保存归属与处置所需的最小事实：不含凭据、手机号、对象内容与请求体。
//! [`Report::leaks`] 用来在回归里证明这一点。

use atlas_cloud::tencent::{CloudOutcome, cos::CosOutcome, sms::SmsOutcome};
use serde_json::json;

use crate::diagnosis::{diagnose_cos, diagnose_sms};

/// 一次云调用的可归档记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// 服务名，例如 `sms`、`cos`。
    pub service: String,
    /// 动作名，例如 `SendSms`。
    pub action: String,
    /// HTTP 状态；结果为业务层时是 `0`，表示该结果类型不携带状态码。
    pub status: u16,
    /// 厂商请求 ID。
    pub request_id: Option<String>,
    /// 结果标签：`sent`、`succeeded`、`rejected`、`indeterminate`。
    pub outcome_label: String,
    /// 调用耗时（毫秒），由调用方计时后传入。
    pub elapsed_ms: u64,
    /// 是否可能已生效、禁止自动重试。
    pub retry_safe: bool,
    /// 请求是否带签名头；结果类型不携带请求头，默认 `false`。
    pub signature_present: bool,
}

impl Record {
    /// 从短信结果构造记录。
    pub fn from_sms(outcome: &SmsOutcome, elapsed_ms: u64) -> Self {
        let (status, outcome_label) = match outcome {
            SmsOutcome::Sent { .. } => (200, "sent"),
            SmsOutcome::Rejected { .. } => (0, "rejected"),
            SmsOutcome::Indeterminate { .. } => (0, "indeterminate"),
        };
        let request_id = match outcome {
            SmsOutcome::Sent { request_id, .. } => Some(request_id.as_str().to_string()),
            SmsOutcome::Rejected { request_id, .. } | SmsOutcome::Indeterminate { request_id, .. } => {
                request_id.as_ref().map(|request_id| request_id.as_str().to_string())
            }
        };
        Self {
            service: "sms".to_string(),
            action: "SendSms".to_string(),
            status,
            request_id,
            outcome_label: outcome_label.to_string(),
            elapsed_ms,
            retry_safe: diagnose_sms(outcome).retry_safe,
            signature_present: false,
        }
    }

    /// 从 COS 结果构造记录。
    ///
    /// COS 结果不携带具体操作，`action` 固定为 `object`，由调用方按调用点覆盖。
    pub fn from_cos(outcome: &CosOutcome, elapsed_ms: u64) -> Self {
        let (status, outcome_label) = match outcome {
            CloudOutcome::Succeeded { .. } => (200, "succeeded"),
            CloudOutcome::Rejected { .. } => (0, "rejected"),
            CloudOutcome::Indeterminate { .. } => (0, "indeterminate"),
        };
        Self {
            service: "cos".to_string(),
            action: "object".to_string(),
            status,
            request_id: outcome.request_id().map(|request_id| request_id.as_str().to_string()),
            outcome_label: outcome_label.to_string(),
            elapsed_ms,
            retry_safe: diagnose_cos(outcome).retry_safe,
            signature_present: false,
        }
    }

    /// 标注本次调用是否带签名头；签名证据从 [`crate::RecordedRequest`] 读出后填入。
    pub fn with_signature(mut self, present: bool) -> Self {
        self.signature_present = present;
        self
    }
}

/// 一次会话内的记录集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    records: Vec<Record>,
}

impl Report {
    /// 创建空报告。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条记录。
    pub fn push(&mut self, record: Record) {
        self.records.push(record);
    }

    /// 每条记录一行 JSON，字段名固定为 `service`、`action`、`status`、`request_id`、
    /// `outcome_label`、`elapsed_ms`、`retry_safe` 与 `signature_present`。
    pub fn to_jsonl(&self) -> String {
        let mut text = String::new();
        for record in &self.records {
            let line = json!({
                "service": record.service,
                "action": record.action,
                "status": record.status,
                "request_id": record.request_id,
                "outcome_label": record.outcome_label,
                "elapsed_ms": record.elapsed_ms,
                "retry_safe": record.retry_safe,
                "signature_present": record.signature_present,
            });
            text.push_str(&line.to_string());
            text.push('\n');
        }
        text
    }

    /// 返回报告里包含任一敏感值的字段名。
    ///
    /// 只扫描字符串字段；空字符串的敏感值被忽略，避免命中一切。同一个字段名只报告一次，
    /// 返回值按首次命中的顺序排列。
    pub fn leaks(&self, secrets: &[&str]) -> Vec<String> {
        let secrets: Vec<&str> = secrets.iter().copied().filter(|secret| !secret.is_empty()).collect();
        if secrets.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<String> = Vec::new();
        for record in &self.records {
            let fields = [
                ("service", record.service.as_str()),
                ("action", record.action.as_str()),
                ("request_id", record.request_id.as_deref().unwrap_or_default()),
                ("outcome_label", record.outcome_label.as_str()),
            ];
            for (name, value) in fields {
                if secrets.iter().any(|secret| value.contains(secret)) && !found.iter().any(|existing| existing == name) {
                    found.push(name.to_string());
                }
            }
        }
        found
    }
}
