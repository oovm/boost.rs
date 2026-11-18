//! `encode_request` 是纯函数：固定输入必须得到固定 JSON，与是否联网无关。

use atlas_cloud::{
    ai::{ChatMessage, ChatRole},
    anthropic, openai,
};
use serde_json::{Value, json};

/// 一轮包含全部三种角色的对话。
fn messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage { role: ChatRole::System, content: "sys".to_string() },
        ChatMessage { role: ChatRole::User, content: "hi".to_string() },
        ChatMessage { role: ChatRole::Assistant, content: "yo".to_string() },
    ]
}

/// 把序列化结果解析成 [`Value`]，比较时不受字段顺序影响。
fn encoded(text: &str) -> Value {
    serde_json::from_str(text).expect("请求体是 JSON")
}

#[test]
fn openai_encodes_model_max_tokens_and_every_message() {
    let client = openai::Client::new("test-key", "https://api.example.com");
    let body = encoded(&client.encode_request("gpt-test", &messages(), 128));

    assert_eq!(
        body,
        json!({
            "model": "gpt-test",
            "max_tokens": 128,
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "yo" },
            ],
        })
    );
}

#[test]
fn anthropic_lifts_system_out_of_messages() {
    let client = anthropic::Client::new("test-key", "https://api.example.com");
    let body = encoded(&client.encode_request("claude-test", &messages(), 128));

    assert_eq!(
        body,
        json!({
            "model": "claude-test",
            "max_tokens": 128,
            "system": "sys",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "yo" },
            ],
        })
    );
}

#[test]
fn anthropic_joins_several_system_messages_with_a_blank_line() {
    let client = anthropic::Client::new("test-key", "https://api.example.com");
    let messages = [
        ChatMessage { role: ChatRole::System, content: "第一段".to_string() },
        ChatMessage { role: ChatRole::System, content: "第二段".to_string() },
        ChatMessage { role: ChatRole::User, content: "hi".to_string() },
    ];
    let body = encoded(&client.encode_request("claude-test", &messages, 16));

    assert_eq!(body["system"], json!("第一段\n\n第二段"));
    assert_eq!(body["messages"], json!([{ "role": "user", "content": "hi" }]));
}

#[test]
fn anthropic_without_system_message_omits_the_field() {
    let client = anthropic::Client::new("test-key", "https://api.example.com");
    let messages = [ChatMessage { role: ChatRole::User, content: "hi".to_string() }];
    let body = encoded(&client.encode_request("claude-test", &messages, 16));

    assert!(body.get("system").is_none(), "{body}");
}

#[test]
fn endpoints_appended_to_the_base_url_without_doubling_the_slash() {
    assert_eq!(openai::Client::new("test-key", "https://api.example.com").endpoint(), "https://api.example.com/v1/chat/completions");
    assert_eq!(openai::Client::new("test-key", "https://api.example.com/").endpoint(), "https://api.example.com/v1/chat/completions");
    assert_eq!(anthropic::Client::new("test-key", "https://api.example.com/").endpoint(), "https://api.example.com/v1/messages");
}
