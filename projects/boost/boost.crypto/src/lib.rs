//! Boost 家族的共享密码基础能力门面。
//!
//! 本 crate 对审计过的纯 Rust 原语做薄封装：hash、HMAC/HKDF、AEAD、ECDSA 与 RSA 签名、
//! X25519 密钥交换、base64 编码，以及秘密材料生命周期类型 [`secret::SecretBytes`]。
//! 模块面保持最小与正交，不承载任何协议逻辑；TLS 1.3 等协议状态机归上层消费者，
//! 只在这里取原语。
//!
//! 安全熵复用 `boost-random` 的宿主安全熵合同：所有密钥生成路径内部走
//! `boost_random::Secure<SystemEntropy>`，不接受固定 seed，不直接触碰平台随机接口。
//! 失败按 [`CryptoError`] 分层：宿主熵、输入、认证三类，调用方不需要解析错误字符串。

#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

pub mod aead;
pub mod ecdsa;
pub mod encoding;
pub mod hash;
pub mod mac;
pub mod rsa;
pub mod secret;
pub mod x25519;

mod entropy;
mod error;

pub use error::{CryptoError, Result};
