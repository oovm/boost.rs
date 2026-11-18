//! 回放顺序：脚本耗尽、请求多余与 fixture 解析都必须显式报错。

use atlas_cloud::tencent::{
    CloudError, Credentials, Region,
    sms::{Client, SendSms, Template},
};
use atlas_harness::{CannedResponse, HarnessError, Replay};
use boost_net::{
    Url,
    http::{Method, Request, Transport},
};

/// 构造一个最小请求。
fn request(method: Method, url: &str) -> Request {
    Request::new(method, Url::parse(url).expect("测试 URL 合法"))
}

#[test]
fn empty_script_reports_the_missing_response() {
    let replay = Replay::new(Vec::new());
    let error = replay.send(&request(Method::Post, "https://sms.tencentcloudapi.com/")).expect_err("没有脚本化响应");
    let message = error.to_string();
    assert!(message.contains("期望第 1 个响应"), "{message}");
    assert!(message.contains("POST"), "{message}");
    assert!(message.contains("https://sms.tencentcloudapi.com/"), "{message}");
    assert!(replay.last_request().is_some(), "失败的请求也应留下现场");
}

#[test]
fn extra_request_after_a_consumed_script_reports_its_position() {
    let replay = Replay::new(vec![CannedResponse::json(200, "{}")]);
    replay.send(&request(Method::Get, "https://example.test/first")).expect("第一个请求有响应");

    let error = replay.send(&request(Method::Put, "https://example.test/second")).expect_err("脚本已耗尽");
    let message = error.to_string();
    assert!(message.contains("期望第 2 个响应"), "{message}");
    assert!(message.contains("PUT"), "{message}");
    assert!(message.contains("https://example.test/second"), "{message}");
    assert_eq!(replay.remaining(), 0);
}

#[test]
fn client_maps_replay_exhaustion_to_indeterminate() {
    let replay = Replay::new(Vec::new());
    let client =
        Client::new(Credentials::new("AKIDEXAMPLE", "secretkey"), "1400000000", Region::ap_guangzhou()).with_transport(Box::new(replay));

    let error = client
        .send(&SendSms {
            phones: vec!["+8613800138000".to_string()], sign_name: "测试签名".to_string(), template: Template::new("1234567")
        })
        .expect_err("没有脚本化响应");
    match error {
        CloudError::Indeterminate { reason, .. } => {
            assert!(reason.contains("POST"), "{reason}");
            assert!(reason.contains("sms.tencentcloudapi.com"), "{reason}");
        }
        other => panic!("回放耗尽应归入 Indeterminate，实际 {other:?}"),
    }
}

#[test]
fn jsonl_fixture_loads_responses_in_order() {
    let text =
        concat!(r#"{"status":200,"body":"{\"ok\":true}"}"#, "\n", r#"{"status":403,"headers":{"x-cos-request-id":"rid"},"body":"denied"}"#,);
    let replay = Replay::from_jsonl(text).expect("fixture 合法");
    assert_eq!(replay.remaining(), 2);
    replay.send(&request(Method::Get, "https://example.test/a")).expect("第一个响应");
    replay.send(&request(Method::Get, "https://example.test/b")).expect("第二个响应");
    assert_eq!(replay.remaining(), 0);
}

#[test]
fn malformed_fixture_reports_the_line_number() {
    let text = "{\"status\":200,\"body\":\"ok\"}\nnot-json\n";
    match Replay::from_jsonl(text) {
        Err(HarnessError::Fixture { line, .. }) => assert_eq!(line, 2),
        other => panic!("应报告第 2 行解析失败，实际 {other:?}"),
    }
}

#[test]
fn fixture_with_an_invalid_header_name_is_rejected() {
    let text = "{\"status\":200,\"headers\":{\"bad header\":\"x\"},\"body\":\"ok\"}";
    match Replay::from_jsonl(text) {
        Err(HarnessError::InvalidHeader { name, .. }) => assert_eq!(name, "bad header"),
        other => panic!("非法头名应被拒绝，实际 {other:?}"),
    }
}

#[test]
fn fixture_without_status_is_rejected() {
    let text = "{\"body\":\"ok\"}";
    match Replay::from_jsonl(text) {
        Err(HarnessError::Fixture { line, detail }) => {
            assert_eq!(line, 1);
            assert!(detail.contains("status"), "{detail}");
        }
        other => panic!("缺少 status 应被拒绝，实际 {other:?}"),
    }
}
