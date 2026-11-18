//! 用回放替身驱动真实的 COS 客户端，覆盖成功与 4xx 拒绝。

use atlas_cloud::tencent::{CloudOutcome, Credentials, Region, cos::Client};
use atlas_harness::{CannedResponse, Replay};

/// 把回放替身装进真实客户端。
fn client(replay: &Replay) -> Client {
    Client::new(Credentials::new("AKIDEXAMPLE", "secretkey"), Region::ap_guangzhou(), "1250000000").with_transport(Box::new(replay.clone()))
}

#[test]
fn replay_drives_put_object_success() {
    let replay = Replay::new(vec![CannedResponse::json(200, "").with_header("x-cos-request-id", "cos-rid-1").with_header("etag", "\"d41d8\"")]);

    let outcome = client(&replay).put_object("bucket", "docs/a.txt", b"hello").expect("回放成功不是错误");
    match outcome {
        CloudOutcome::Succeeded { value, request_id } => {
            assert_eq!(request_id.as_str(), "cos-rid-1");
            assert_eq!(value.etag.as_deref(), Some("d41d8"));
            assert!(value.body.is_empty(), "PUT 没有响应体");
        }
        other => panic!("期望成功，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);

    let request = replay.last_request().expect("回放应记录请求");
    assert_eq!(request.method(), "PUT");
    assert!(request.url().contains("bucket-1250000000.cos.ap-guangzhou.myqcloud.com/docs/a.txt"), "{}", request.url());
    let authorization = request.header("authorization").expect("签名头存在");
    assert!(authorization.contains("q-sign-algorithm=sha1"), "{authorization}");
    assert_eq!(request.body(), "hello");
}

#[test]
fn replay_drives_put_object_rejection() {
    let replay =
        Replay::new(vec![CannedResponse::json(403, "<Error><Code>AccessDenied</Code></Error>").with_header("x-cos-request-id", "cos-rid-2")]);

    let outcome = client(&replay).put_object("bucket", "docs/a.txt", b"hello").expect("非 2xx 是明确拒绝");
    match outcome {
        CloudOutcome::Rejected { code, message, request_id } => {
            assert_eq!(code, "HTTP_403");
            assert!(message.contains("AccessDenied"), "{message}");
            assert_eq!(request_id.expect("有请求 ID").as_str(), "cos-rid-2");
        }
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
    assert_eq!(replay.remaining(), 0);
}
