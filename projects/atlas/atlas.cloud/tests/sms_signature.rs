//! SMS TC3-HMAC-SHA256 签名回归。
//!
//! 期望值由一份独立实现（Python 的 `hashlib` / `hmac`，不经过被测代码）按腾讯云公开的
//! TC3 公式算出，推导过程：
//!
//! 1. 规范请求 = `POST` + `\n` + `/` + `\n` + 空查询串 + `\n` + 小写头
//!    `content-type:application/json; charset=utf-8` 与 `host:sms.tencentcloudapi.com`
//!    （各自后跟 `\n`）+ 空行 + 签名头列表 `content-type;host` + `\n` + `hex(sha256(payload))`；
//! 2. 凭证范围 = `<UTC 日期>/sms/tc3_request`，`1700000000` 对应 `2023-11-14`；
//! 3. `StringToSign` = `TC3-HMAC-SHA256\n<timestamp>\n<scope>\n<hex(sha256(规范请求))>`；
//! 4. 派生密钥依次为 `HMAC(("TC3" + SecretKey), 日期)` → `HMAC(·, "sms")` →
//!    `HMAC(·, "tc3_request")`；
//! 5. 最终 `Signature` = `hex(HMAC(派生密钥, StringToSign))`。
//!
//! 中间值同样来自那次独立计算（`payload` 的 SHA-256 为
//! `105fc42652b6d041ce0e659cb7e2c7b2cf303bd9adece15c8ec868c5c6c7b7f2`，规范请求的
//! SHA-256 为 `8ba49f391ed7e26d48353f235400dfb5b4fd38e57daa3a95f9aebd3741ceedfc`）。

use atlas_cloud::tencent::{Credentials, Region, sms::Client};

/// 固定 payload：字段顺序与 `Client::send` 内部的序列化结果无关，签名只绑定这份字节。
const PAYLOAD: &str = r#"{"PhoneNumberSet":["+8613800138000"],"SignName":"Boost","SmsSdkAppId":"1400000000","TemplateId":"1234567","TemplateParamSet":["123456"]}"#;
/// 固定时间戳：2023-11-14T22:13:20Z。
const TIMESTAMP: u64 = 1_700_000_000;

fn client() -> Client {
    Client::new(Credentials::new("AKIDEXAMPLEEXAMPLEEXAMPLE", "SECRETKEYEXAMPLEEXAMPLE"), "1400000000", Region::ap_guangzhou())
}

#[test]
fn authorization_matches_independent_derivation() {
    let expected = concat!(
        "TC3-HMAC-SHA256 Credential=AKIDEXAMPLEEXAMPLEEXAMPLE/2023-11-14/sms/tc3_request, ",
        "SignedHeaders=content-type;host, ",
        "Signature=811205e7b2501d0daadd91492378e710104b750aa3d2050a4eb4e41136f36358"
    );
    assert_eq!(client().authorization(PAYLOAD, TIMESTAMP), expected);
}

#[test]
fn authorization_is_deterministic_and_bound_to_payload_and_clock() {
    let client = client();
    assert_eq!(client.authorization(PAYLOAD, TIMESTAMP), client.authorization(PAYLOAD, TIMESTAMP));
    assert_ne!(client.authorization(PAYLOAD, TIMESTAMP), client.authorization(PAYLOAD, TIMESTAMP + 1));
    assert_ne!(client.authorization("{}", TIMESTAMP), client.authorization(PAYLOAD, TIMESTAMP));
}

#[test]
fn credentials_debug_redacts_both_fields() {
    let debug = format!("{:?}", Credentials::new("AKIDSECRETEXAMPLE", "SECRETKEYEXAMPLE"));
    assert!(!debug.contains("AKIDSECRETEXAMPLE"), "{debug}");
    assert!(!debug.contains("SECRETKEYEXAMPLE"), "{debug}");
    assert_eq!(debug.matches("[redacted]").count(), 2, "{debug}");
}

#[test]
fn region_is_a_plain_name_wrapper() {
    assert_eq!(Region::ap_guangzhou().as_str(), "ap-guangzhou");
    assert_eq!(Region::new("ap-shanghai").to_string(), "ap-shanghai");
}
