//! COS `q-sign-algorithm=sha1` 签名的结构性质。
//!
//! 这里不比对官方示例向量：本实现尚未与腾讯云官方示例对齐，因此只断言公式自洽可以观察到
//! 的部分——`KeyTime` 格式、`q-header-list` 的形态、同输入稳定、改路径即改签名。等拿到官方
//! 向量后应在此补一条逐字节比对的用例。

use atlas_cloud::tencent::{
    Credentials, Region,
    cos::{Client, KeyTime},
};

const KEY_TIME: KeyTime = KeyTime::new(1_700_000_000, 1_700_000_300);

fn client() -> Client {
    Client::new(Credentials::new("AKIDEXAMPLEEXAMPLEEXAMPLE", "SECRETKEYEXAMPLEEXAMPLE"), Region::ap_guangzhou(), "1250000000")
}

#[test]
fn key_time_formats_as_unix_second_pair() {
    assert_eq!(KEY_TIME.to_string(), "1700000000;1700000300");
}

#[test]
fn signature_is_stable_and_hex_encoded() {
    let client = client();
    let first = client.signature("example", "GET", "/reports/2026-11.json", &KEY_TIME);
    let second = client.signature("example", "GET", "/reports/2026-11.json", &KEY_TIME);
    assert_eq!(first, second);
    // HMAC-SHA1 的十六进制是 40 个小写字符。
    assert_eq!(first.len(), 40, "{first}");
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()), "{first}");
}

#[test]
fn signature_changes_with_path_method_and_key_time() {
    let client = client();
    let base = client.signature("example", "GET", "/reports/2026-11.json", &KEY_TIME);
    assert_ne!(base, client.signature("example", "GET", "/reports/2026-12.json", &KEY_TIME));
    assert_ne!(base, client.signature("example", "HEAD", "/reports/2026-11.json", &KEY_TIME));
    assert_ne!(base, client.signature("example", "GET", "/reports/2026-11.json", &KeyTime::new(1_700_000_000, 1_700_000_301)));
    // 桶名参与访问域名，因而参与签名。
    assert_ne!(base, client.signature("other", "GET", "/reports/2026-11.json", &KEY_TIME));
}

#[test]
fn authorization_lists_host_as_the_only_signed_header() {
    let client = client();
    let authorization = client.authorization("example", "PUT", "/a.txt", &KEY_TIME);
    let signature = client.signature("example", "PUT", "/a.txt", &KEY_TIME);

    assert!(authorization.starts_with("q-sign-algorithm=sha1&q-ak=AKIDEXAMPLEEXAMPLEEXAMPLE&"), "{authorization}");
    assert!(authorization.contains("&q-sign-time=1700000000;1700000300&"), "{authorization}");
    assert!(authorization.contains("&q-key-time=1700000000;1700000300&"), "{authorization}");
    // 只签 `host` 一个头，因此列表既是小写也天然有序。
    assert!(authorization.contains("&q-header-list=host&"), "{authorization}");
    assert!(authorization.contains("&q-url-param-list=&"), "{authorization}");
    assert!(authorization.ends_with(&format!("&q-signature={signature}")), "{authorization}");
}
