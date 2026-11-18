//! 报告的 JSONL 投影与敏感值回归。

use atlas_cloud::tencent::{CloudOutcome, RequestId, cos::CosObject, sms::SmsOutcome};
use atlas_harness::{Record, Report};
use serde_json::Value;

/// 一条成功下发的短信结果。
fn sent() -> SmsOutcome {
    SmsOutcome::Sent { serial_no: "5000:1".to_string(), request_id: RequestId::new("rid-sent") }
}

/// 一条成功的 COS 结果。
fn stored() -> CloudOutcome<CosObject> {
    CloudOutcome::Succeeded { value: CosObject::default(), request_id: RequestId::new("cos-rid-1") }
}

#[test]
fn jsonl_has_one_stable_line_per_record() {
    let mut report = Report::new();
    report.push(Record::from_sms(&sent(), 12).with_signature(true));
    report.push(Record::from_cos(&stored(), 30));

    let text = report.to_jsonl();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(text.ends_with('\n'), "JSONL 以换行结束");

    for line in &lines {
        let value: Value = serde_json::from_str(line).expect("每行都是 JSON 对象");
        let object = value.as_object().expect("顶层是对象");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["action", "elapsed_ms", "outcome_label", "request_id", "retry_safe", "service", "signature_present", "status"]);
    }

    let first: Value = serde_json::from_str(lines[0]).expect("第一行是 JSON");
    assert_eq!(first["service"], "sms");
    assert_eq!(first["action"], "SendSms");
    assert_eq!(first["status"], 200);
    assert_eq!(first["outcome_label"], "sent");
    assert_eq!(first["request_id"], "rid-sent");
    assert_eq!(first["elapsed_ms"], 12);
    assert_eq!(first["retry_safe"], false);
    assert_eq!(first["signature_present"], true);

    let second: Value = serde_json::from_str(lines[1]).expect("第二行是 JSON");
    assert_eq!(second["service"], "cos");
    assert_eq!(second["outcome_label"], "succeeded");
    assert_eq!(second["request_id"], "cos-rid-1");
}

#[test]
fn leaks_reports_the_fields_that_contain_injected_secrets() {
    let phone = "+8613800138000";
    let secret_key = "super-secret-key";
    let mut record = Record::from_sms(&sent(), 1);
    record.request_id = Some(phone.to_string());
    record.outcome_label = secret_key.to_string();

    let mut report = Report::new();
    report.push(record);

    let found = report.leaks(&[secret_key, phone]);
    assert!(found.contains(&"outcome_label".to_string()), "{found:?}");
    assert!(found.contains(&"request_id".to_string()), "{found:?}");
}

#[test]
fn clean_report_does_not_report_leaks() {
    let mut report = Report::new();
    report.push(Record::from_sms(&sent(), 3));
    report.push(Record::from_cos(&stored(), 4));

    assert!(report.leaks(&["+8613800138000", "secretkey"]).is_empty());
    assert!(report.leaks(&[""]).is_empty(), "空敏感值不应命中任何字段");
}
