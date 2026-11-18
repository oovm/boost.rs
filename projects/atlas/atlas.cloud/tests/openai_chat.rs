//! 用回放替身驱动真实的 OpenAI 兼容客户端，覆盖成功、拒绝、限流与结构不符四类结果。

use atlas_cloud::{
    ai::{ChatMessage, ChatOutcome, ChatRole},
    openai::Client,
};
use atlas_harness::{CannedResponse, Replay};
use serde_json::{Value, json};

/// 把回放替身装进真实客户端。
fn client(replay: &Replay) -> Client {
    Client::new("test-key", "https://api.example.com").with_transport(Box::new(replay.clone()))
}

/// 固定的两轮对话。
fn messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage { role: ChatRole::System, content: "你是助手".to_string() },
        ChatMessage { role: ChatRole::User, content: "你好".to_string() },
    ]
}

#[test]
fn replay_drives_completed_and_records_the_request() {
    let body = r#"{"id":"chatcmpl-1","choices":[{"message":{"role":"assistant","content":"pong"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body)]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 256).expect("回放成功不是错误");
    match outcome {
        ChatOutcome::Completed { text, usage, request_id } => {
            assert_eq!(text, "pong");
            assert_eq!(usage.input_tokens, 3);
            assert_eq!(usage.output_tokens, 2);
            assert_eq!(request_id.expect("响应体 id 兜底为请求 ID").as_str(), "chatcmpl-1");
        }
        other => panic!("期望 Completed，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);

    let request = replay.last_request().expect("回放应记录请求");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url(), "https://api.example.com/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.header("content-type"), Some("application/json"));

    let payload: Value = serde_json::from_str(request.body()).expect("请求体是 JSON");
    assert_eq!(payload["model"], json!("gpt-test"));
    assert_eq!(payload["max_tokens"], json!(256));
    assert_eq!(payload["messages"][0], json!({ "role": "system", "content": "你是助手" }));
    assert_eq!(payload["messages"][1], json!({ "role": "user", "content": "你好" }));
}

#[test]
fn response_header_request_id_wins_over_body_id() {
    let body = r#"{"id":"chatcmpl-body","choices":[{"message":{"content":"pong"}}],"usage":{}}"#;
    let replay = Replay::new(vec![CannedResponse::json(200, body).with_header("x-request-id", "req-header")]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("回放成功不是错误");
    match outcome {
        ChatOutcome::Completed { request_id, .. } => {
            assert_eq!(request_id.expect("头里的请求 ID").as_str(), "req-header");
        }
        other => panic!("期望 Completed，实际 {other:?}"),
    }
}

#[test]
fn error_body_is_rejected_with_vendor_code() {
    let body = r#"{"error":{"message":"model not found","type":"invalid_request_error","code":"model_not_found"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(400, body)]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("拒绝是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, message, request_id, retry_after } => {
            assert_eq!(status, 400);
            assert_eq!(code, "model_not_found");
            assert_eq!(message, "model not found");
            assert_eq!(request_id, None);
            assert_eq!(retry_after, None);
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn error_without_code_falls_back_to_type() {
    let body = r#"{"error":{"message":"invalid api key","type":"authentication_error","code":null}}"#;
    let replay = Replay::new(vec![CannedResponse::json(401, body)]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("拒绝是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, message, .. } => {
            assert_eq!(status, 401);
            assert_eq!(code, "authentication_error");
            assert_eq!(message, "invalid api key");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn rate_limit_keeps_retry_after_seconds() {
    let body = r#"{"error":{"message":"rate limited","type":"requests","code":"rate_limit_exceeded"}}"#;
    let replay = Replay::new(vec![CannedResponse::json(429, body).with_header("retry-after", "7")]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("限流是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, retry_after, .. } => {
            assert_eq!(status, 429);
            assert_eq!(code, "rate_limit_exceeded");
            assert_eq!(retry_after, Some(7));
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn non_json_failure_derives_the_http_code() {
    let replay = Replay::new(vec![CannedResponse::json(502, "<html>bad gateway</html>")]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("明确拒绝是结果而不是错误");
    match outcome {
        ChatOutcome::Rejected { status, code, message, .. } => {
            assert_eq!(status, 502);
            assert_eq!(code, "HTTP_502");
            assert!(message.contains("bad gateway"), "{message}");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

#[test]
fn malformed_success_body_is_indeterminate() {
    let replay = Replay::new(vec![CannedResponse::json(200, r#"{"choices":[]}"#)]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("请求已送达，不能折叠成普通失败");
    match outcome {
        ChatOutcome::Indeterminate { reason, request_id } => {
            assert!(reason.contains("choices[0].message.content"), "{reason}");
            assert_eq!(request_id, None);
        }
        other => panic!("结构不符的成功响应应为 Indeterminate，实际 {other:?}"),
    }
}

#[test]
fn non_json_success_body_is_indeterminate() {
    let replay = Replay::new(vec![CannedResponse::json(200, "OK")]);

    let outcome = client(&replay).chat("gpt-test", &messages(), 16).expect("请求已送达，不能折叠成普通失败");
    assert!(matches!(&outcome, ChatOutcome::Indeterminate { .. }), "实际 {outcome:?}");
    assert!(!outcome.is_completed());
}
