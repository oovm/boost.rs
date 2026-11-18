//! 响应分层：明确拒绝、业务失败、结果不确定，以及缺少关键字段时的确定结果。
//!
//! 这些用例只用构造的响应字节与头，不联网；它们锁定的是 `interpret_response` 的分层规则，
//! 而不是腾讯云的线上行为。

use atlas_cloud::tencent::{
    CloudError, CloudOutcome, RequestId,
    cos::interpret_response as interpret_cos_response,
    sms::{SmsOutcome, interpret_response},
};
use boost_net::http::Headers;

/// 断言结果是成功下发。
fn expect_sent(outcome: SmsOutcome) -> (String, String) {
    match outcome {
        SmsOutcome::Sent { serial_no, request_id } => (serial_no, request_id.as_str().to_string()),
        other => panic!("期望 Sent，实际 {other:?}"),
    }
}

/// 断言结果是明确拒绝。
fn expect_rejected(outcome: SmsOutcome) -> (String, String, Option<RequestId>) {
    match outcome {
        SmsOutcome::Rejected { code, message, request_id } => (code, message, request_id),
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn business_error_inside_2xx_is_rejected_with_request_id() {
    let body = r#"{"Response":{"Error":{"Code":"FailedOperation.InsufficientBalanceInSmsPackage","Message":"套餐余量不足"},"RequestId":"8f1d2c3b-4a5e-6789-abcd-ef0123456789"}}"#;
    let (code, message, request_id) = expect_rejected(interpret_response(200, body.as_bytes()).expect("结构化拒绝不是错误"));
    assert_eq!(code, "FailedOperation.InsufficientBalanceInSmsPackage");
    assert_eq!(message, "套餐余量不足");
    assert_eq!(request_id.expect("有 RequestId").as_str(), "8f1d2c3b-4a5e-6789-abcd-ef0123456789");
}

#[test]
fn per_phone_failure_is_rejected() {
    let body = r#"{"Response":{"SendStatusSet":[{"Code":"LimitExceeded.PhoneNumberThirtySecondLimit","Message":"单号码三十秒限频","SerialNo":""}],"RequestId":"rid-1"}}"#;
    let (code, message, request_id) = expect_rejected(interpret_response(200, body.as_bytes()).expect("业务码失败是结果而不是错误"));
    assert_eq!(code, "LimitExceeded.PhoneNumberThirtySecondLimit");
    assert_eq!(message, "单号码三十秒限频");
    assert_eq!(request_id.expect("有 RequestId").as_str(), "rid-1");
}

#[test]
fn accepted_send_carries_serial_no_and_request_id() {
    let body = r#"{"Response":{"SendStatusSet":[{"Code":"Ok","Message":"send success","SerialNo":"5000:1234567890"}],"RequestId":"rid-2"}}"#;
    let (serial_no, request_id) = expect_sent(interpret_response(200, body.as_bytes()).expect("成功响应"));
    assert_eq!(serial_no, "5000:1234567890");
    assert_eq!(request_id, "rid-2");
}

#[test]
fn accepted_send_without_request_id_is_not_faked_as_success() {
    let body = r#"{"Response":{"SendStatusSet":[{"Code":"Ok","SerialNo":"5000:1234567890"}]}}"#;
    match interpret_response(200, body.as_bytes()) {
        Err(CloudError::Decode(detail)) => assert!(detail.contains("RequestId"), "{detail}"),
        other => panic!("缺少 RequestId 不得返回成功，实际 {other:?}"),
    }
}

#[test]
fn accepted_send_without_serial_no_is_a_decode_error() {
    let body = r#"{"Response":{"SendStatusSet":[{"Code":"Ok","Message":"send success"}],"RequestId":"rid-3"}}"#;
    match interpret_response(200, body.as_bytes()) {
        Err(CloudError::Decode(detail)) => assert!(detail.contains("SerialNo"), "{detail}"),
        other => panic!("缺少 SerialNo 不得返回成功，实际 {other:?}"),
    }
}

#[test]
fn http_failure_with_provider_body_is_rejected_not_error() {
    let body = r#"{"Response":{"Error":{"Code":"AuthFailure.SignatureFailure","Message":"签名错误"},"RequestId":"rid-4"}}"#;
    let (code, message, request_id) = expect_rejected(interpret_response(403, body.as_bytes()).expect("厂商错误结构可结构化表达"));
    assert_eq!(code, "AuthFailure.SignatureFailure");
    assert_eq!(message, "签名错误");
    assert_eq!(request_id.expect("有 RequestId").as_str(), "rid-4");
}

#[test]
fn http_failure_without_provider_body_stays_rejected() {
    match interpret_response(502, b"<html>bad gateway</html>") {
        Err(CloudError::Rejected { status, code, message, request_id }) => {
            assert_eq!(status, 502);
            assert_eq!(code, "HTTP_502");
            assert!(message.contains("bad gateway"), "{message}");
            assert_eq!(request_id, None);
        }
        other => panic!("网关错误应保留为 Rejected，实际 {other:?}"),
    }
}

#[test]
fn undecodable_success_body_is_indeterminate() {
    match interpret_response(200, b"OK") {
        Ok(SmsOutcome::Indeterminate { reason, request_id }) => {
            assert!(reason.contains("2xx"), "{reason}");
            assert_eq!(request_id, None);
        }
        other => panic!("请求已送达但无法解码时应为 Indeterminate，实际 {other:?}"),
    }
}

#[test]
fn json_without_provider_envelope_is_indeterminate() {
    match interpret_response(200, br#"{"hello":"world"}"#) {
        Ok(SmsOutcome::Indeterminate { reason, .. }) => assert!(reason.contains("腾讯云"), "{reason}"),
        other => panic!("不是腾讯云响应时应为 Indeterminate，实际 {other:?}"),
    }
}

#[test]
fn empty_phone_status_set_is_a_contract_violation() {
    match interpret_response(200, br#"{"Response":{"SendStatusSet":[],"RequestId":"rid-5"}}"#) {
        Err(CloudError::Decode(detail)) => assert!(detail.contains("SendStatusSet"), "{detail}"),
        other => panic!("缺少 SendStatusSet[0] 应解码失败，实际 {other:?}"),
    }
}

/// 构造响应头。
fn headers(pairs: &[(&str, &str)]) -> Headers {
    let mut headers = Headers::new();
    for (name, value) in pairs {
        headers.set(*name, *value);
    }
    headers
}

#[test]
fn cos_success_keeps_request_id_and_object_metadata() {
    let headers =
        headers(&[("x-cos-request-id", "cos-rid-1"), ("content-type", "application/json"), ("content-length", "2"), ("etag", "\"d41d8\"")]);
    match interpret_cos_response(200, &headers, b"{}") {
        Ok(CloudOutcome::Succeeded { value, request_id }) => {
            assert_eq!(request_id.as_str(), "cos-rid-1");
            assert_eq!(value.body, b"{}");
            assert_eq!(value.content_type.as_deref(), Some("application/json"));
            assert_eq!(value.content_length, Some(2));
            assert_eq!(value.etag.as_deref(), Some("d41d8"));
        }
        other => panic!("2xx 应成功，实际 {other:?}"),
    }
}

#[test]
fn cos_success_without_request_id_is_not_faked() {
    match interpret_cos_response(200, &headers(&[("content-type", "text/plain")]), b"ok") {
        Err(CloudError::Decode(detail)) => assert!(detail.contains("x-cos-request-id"), "{detail}"),
        other => panic!("缺少请求 ID 不得返回成功，实际 {other:?}"),
    }
}

#[test]
fn cos_http_failure_is_rejected_with_header_request_id() {
    let headers = headers(&[("x-cos-request-id", "cos-rid-2")]);
    let body = br#"<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#;
    match interpret_cos_response(404, &headers, body) {
        Ok(CloudOutcome::Rejected { code, message, request_id }) => {
            assert_eq!(code, "HTTP_404");
            assert!(message.contains("NoSuchKey"), "{message}");
            assert_eq!(request_id.expect("有请求 ID").as_str(), "cos-rid-2");
        }
        other => panic!("非 2xx 应明确拒绝，实际 {other:?}"),
    }
}
