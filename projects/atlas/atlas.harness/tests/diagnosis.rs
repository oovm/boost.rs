//! 诊断语义：分层、重试安全性、对账动作与已生效标记。

use atlas_cloud::tencent::{CloudError, CloudOutcome, RequestId, cos::CosOutcome, sms::SmsOutcome};
use atlas_harness::{Layer, diagnose_cos, diagnose_error, diagnose_sms};

#[test]
fn indeterminate_error_is_never_retry_safe() {
    let error =
        CloudError::Indeterminate { reason: "传输层失败：响应读取中断".to_string(), request_id: Some(RequestId::new("rid-1")) };
    let diagnosis = diagnose_error(&error);
    assert_eq!(diagnosis.layer, Layer::Transport);
    assert!(!diagnosis.retry_safe, "可能已生效的请求不能标记为可重试");
    assert_eq!(diagnosis.request_id.as_deref(), Some("rid-1"));
    assert!(!diagnosis.next_steps.is_empty());
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("对账")), "{:?}", diagnosis.next_steps);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("不要自动重试")), "{:?}", diagnosis.next_steps);
}

#[test]
fn indeterminate_sms_outcome_points_at_transport_and_reconciliation() {
    let outcome = SmsOutcome::Indeterminate { reason: "传输层失败：连接被重置".to_string(), request_id: None };
    let diagnosis = diagnose_sms(&outcome);
    assert_eq!(diagnosis.layer, Layer::Transport);
    assert!(!diagnosis.retry_safe);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("不要自动重试")), "{:?}", diagnosis.next_steps);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("对账")), "{:?}", diagnosis.next_steps);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("幂等")), "{:?}", diagnosis.next_steps);
}

#[test]
fn invalid_config_names_the_missing_environment_variable() {
    let diagnosis = diagnose_error(&CloudError::InvalidConfig("TENCENTCLOUD_SECRET_ID".to_string()));
    assert_eq!(diagnosis.layer, Layer::Config);
    assert!(diagnosis.retry_safe, "配置缺失时请求没有发出，可以安全重试");
    assert_eq!(diagnosis.request_id, None);
    assert!(diagnosis.summary.contains("TENCENTCLOUD_SECRET_ID"), "{}", diagnosis.summary);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("TENCENTCLOUD_SECRET_ID")), "{:?}", diagnosis.next_steps);
}

#[test]
fn sent_sms_is_marked_as_already_delivered() {
    let outcome = SmsOutcome::Sent { serial_no: "5000:1".to_string(), request_id: RequestId::new("rid-sent") };
    let diagnosis = diagnose_sms(&outcome);
    assert_eq!(diagnosis.layer, Layer::Vendor);
    assert!(!diagnosis.retry_safe, "已下发的短信不能重发");
    assert_eq!(diagnosis.request_id.as_deref(), Some("rid-sent"));
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("不要重发")), "{:?}", diagnosis.next_steps);
}

#[test]
fn vendor_retry_safety_depends_on_the_error_class() {
    let quota = SmsOutcome::Rejected {
        code: "FailedOperation.InsufficientBalanceInSmsPackage".to_string(),
        message: "套餐余量不足".to_string(),
        request_id: Some(RequestId::new("rid-quota")),
    };
    assert!(diagnose_sms(&quota).retry_safe, "额度类错误修正后可以重试");

    let unknown = SmsOutcome::Rejected {
        code: "FailedOperation.InternalError".to_string(),
        message: "内部错误".to_string(),
        request_id: Some(RequestId::new("rid-unknown")),
    };
    let diagnosis = diagnose_sms(&unknown);
    assert_eq!(diagnosis.layer, Layer::Vendor);
    assert!(!diagnosis.retry_safe, "未知业务错误不得假设可以重试");
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("不要按同一请求直接重试")), "{:?}", diagnosis.next_steps);
}

#[test]
fn cos_indeterminate_mentions_put_idempotency_without_generalizing() {
    let outcome: CosOutcome = CloudOutcome::Indeterminate { reason: "传输层失败：超时".to_string(), request_id: None };
    let diagnosis = diagnose_cos(&outcome);
    assert_eq!(diagnosis.layer, Layer::Transport);
    assert!(!diagnosis.retry_safe);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("同 key 同内容重放安全")), "{:?}", diagnosis.next_steps);
    assert!(
        diagnosis.next_steps.iter().any(|step| step.contains("不要套用")),
        "结论必须限定在 PUT，不能泛化到 GET/HEAD：{:?}",
        diagnosis.next_steps
    );
}

#[test]
fn decode_error_is_a_protocol_layer_problem() {
    let diagnosis = diagnose_error(&CloudError::Decode("响应缺少 SerialNo".to_string()));
    assert_eq!(diagnosis.layer, Layer::Protocol);
    assert!(!diagnosis.retry_safe);
    assert!(diagnosis.next_steps.iter().any(|step| step.contains("不要当作成功")), "{:?}", diagnosis.next_steps);
}
