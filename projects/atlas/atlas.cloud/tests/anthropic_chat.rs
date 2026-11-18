//! 用回放替身驱动真实的 Anthropic messages 客户端，覆盖成功、拒绝、限流与结构不符四类结果。

use atlas_cloud::{
    ai::{ChatMessage, ChatOutcome, ChatRole},
    anthropic::Client,
};
use atlas_harness::{CannedResponse, Replay};
use serde_json::{Value, json};

/// 把回放替身装进真实客户端。
fn client(replay: &Replay) -> Client {
    Client::new("test-key", "https://api.example.com").with_transport(Box::new(replay.clone()))
}

/// 一条系统提示加一条用户输入。
fn messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage { role: ChatRole::System, content: "你是助手".to_string() },
        ChatMessage { role: ChatRole::User, content: "你好".to_string() },
    ]
}

#[test]
fn replay_drives_completed_and_records_the_request() {
    let body = r#"{"id":"msg-1","content":[{"type":"text","text":"po"},{"type":"tool_use","id":"x"},{"type":"text","text":"ng"}],"usage":{"input_tokens":4,"output_tokens":5}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body)]);

    let outcome = client(&replay).chat("claude-test", &messages(), 256).expect("回放成功不是错误");
    match outcome {
        ChatOutcome::Completed { text, usage, request_id } => {
            assert_eq!(text, "pong");
            assert_eq!(usage.input_tokens, 4);
            assert_eq!(usage.output_tokens, 5);
            assert_eq!(request_id.expect("响应体 id 兜底为请求 ID").as_str(), "msg-1");
        }
        other => panic!("期望 Completed，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);

    let request = replay.last_request().expect("回放应记录请求");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url(), "https://api.example.com/v1/messages");
    assert_eq!(request.header("x-api-key"), Some("test-key"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("content-type"), Some("application/json"));

    let payload: Value = serde_json::from_str(request.body()).expect("请求体是 JSON");
    assert_eq!(payload["model"], json!("claude-test"));
    assert_eq!(payload["max_tokens"], json!(256));
    assert_eq!(payload["system"], json!("你是助手"));
    assert_eq!(payload["messages"], json!([{ "role": "user", "content": "你好" }]));
}

#[test]
fn response_header_request_id_wins_over_body_id() {
    let body = r#"{"id":"msg-body","content":[{"type":"text","text":"pong"}],"usage":{}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body).with_header("request-id", "req-header")]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("回放成功不是错误");
    match outcome {
        ChatOutcome::Completed { request_id, .. } => {
            assert_eq!(request_id.expect("头里的请求 ID").as_str(), "req-header");
        }
        other => panic!("期望 Completed，实际 {other:?}"),
    }
}

#[test]
fn error_body_is_rejected_with_vendor_type() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(400, body)]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("拒绝是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, message, request_id, retry_after } => {
            assert_eq!(status, 400);
            assert_eq!(code, "invalid_request_error");
            assert_eq!(message, "max_tokens: must be greater than 0");
            assert_eq!(request_id, None);
            assert_eq!(retry_after, None);
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn rate_limit_keeps_retry_after_seconds() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"number of requests exceeded"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(429, body).with_header("retry-after", "7")]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("限流是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, retry_after, .. } => {
            assert_eq!(status, 429);
            assert_eq!(code, "rate_limit_error");
            assert_eq!(retry_after, Some(7));
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn non_json_failure_derives_the_http_code() {
    let replay = Replay::new(vec![CannedResponse::json(503, "<html>service unavailable</html>")]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("明确拒绝是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, message, .. } => {
            assert_eq!(status, 503);
            assert_eq!(code, "HTTP_503");
            assert!(message.contains("service unavailable"), "{message}");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn malformed_success_body_is_indeterminate() {
    let replay = Replay::new(vec![CannedResponse::json(200, r#"{"content":[]}"#)]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("请求已送达，不能折叠成普通失败");
    match outcome {
        ChatOutcome::Indeterminate { reason, request_id } => {
            assert!(reason.contains("text"), "{reason}");
            assert_eq!(request_id, None);
        }
        other => panic!("没有 text 块的成功响应应为 Indeterminate，实际 {other:?}"),
    }
}

#[test]
fn success_without_content_array_is_indeterminate() {
    let replay = Replay::new(vec![CannedResponse::json(200, r#"{"id":"msg-2"}"#)]);

    let outcome = client(&replay).chat("claude-test", &messages(), 16).expect("请求已送达，不能折叠成普通失败");
    match outcome {
        ChatOutcome::Indeterminate { reason, request_id } => {
            assert!(reason.contains("content"), "{reason}");
            assert_eq!(request_id.expect("响应体 id 仍可用于对账").as_str(), "msg-2");
        }
        other => panic!("缺少 content 数组应为 Indeterminate，实际 {other:?}"),
    }
}
