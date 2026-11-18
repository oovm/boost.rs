//! 用回放替身驱动真实的短信客户端，覆盖三种结果与签名请求的形状。

use atlas_cloud::tencent::{
    Credentials, Region,
    sms::{Client, SendSms, SmsOutcome, Template},
};
use atlas_harness::{CannedResponse, Replay};

/// 把回放替身装进真实客户端。
fn client(replay: &Replay) -> Client {
    Client::new(Credentials::new("AKIDEXAMPLE", "secretkey"), "1400000000", Region::ap_guangzhou()).with_transport(Box::new(replay.clone()))
}

/// 一条固定短信。
fn message() -> SendSms {
    SendSms {
        phones: vec!["+8613800138000".to_string()],
        sign_name: "测试签名".to_string(),
        template: Template::new("1234567").param("654321"),
    }
}

#[test]
fn replay_drives_sent_and_records_the_signed_request() {
    let body = r#"{"Response":{"SendStatusSet":[{"Code":"Ok","Message":"send success","SerialNo":"5000:1"}],"RequestId":"rid-sent"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body)]);

    let outcome = client(&replay).send(&message()).expect("回放成功不是错误");
    match outcome {
        SmsOutcome::Sent { serial_no, request_id } => {
            assert_eq!(serial_no, "5000:1");
            assert_eq!(request_id.as_str(), "rid-sent");
        }
        other => panic!("期望 Sent，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);

    let request = replay.last_request().expect("回放应记录请求");
    assert_eq!(request.method(), "POST");
    assert!(request.url().contains("sms.tencentcloudapi.com"), "{}", request.url());
    let authorization = request.header("authorization").expect("签名头存在");
    assert!(authorization.starts_with("TC3-HMAC-SHA256 Credential="), "{authorization}");
    let payload = request.body();
    assert!(payload.contains("+8613800138000"), "{payload}");
    assert!(payload.contains("654321"), "{payload}");
}

#[test]
fn replay_drives_business_rejection() {
    let body = r#"{"Response":{"Error":{"Code":"LimitExceeded.PhoneNumberThirtySecondLimit","Message":"单号码三十秒限频"},"RequestId":"rid-rejected"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body)]);

    let outcome = client(&replay).send(&message()).expect("业务拒绝是结果而不是错误");
    match outcome {
        SmsOutcome::Rejected { code, message, request_id } => {
            assert_eq!(code, "LimitExceeded.PhoneNumberThirtySecondLimit");
            assert_eq!(message, "单号码三十秒限频");
            assert_eq!(request_id.expect("有 RequestId").as_str(), "rid-rejected");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);
}

#[test]
fn replay_drives_indeterminate_for_an_unrecognized_2xx_body() {
    let replay = Replay::new(vec![CannedResponse::json(200, "OK")]);

    let outcome = client(&replay).send(&message()).expect("请求已送达，不能折叠成普通失败");
    match outcome {
        SmsOutcome::Indeterminate { reason, request_id } => {
            assert!(reason.contains("2xx"), "{reason}");
            assert_eq!(request_id, None);
        }
        other => panic!("无法解码的成功响应应为 Indeterminate，实际 {other:?}"),
    }
}
