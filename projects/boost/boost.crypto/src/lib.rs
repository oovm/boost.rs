//! Boost 家族的共享密码基础能力门面。
//!
//! 本 crate 对审计过的纯 Rust 原语做能力门面：hash、HMAC/HKDF、PBKDF2、AEAD、ECDSA 与 RSA 验签、
//! RSA OAEP 加密、X25519 密钥交换、base64 编码，以及秘密材料生命周期类型 [`secret::SecretBytes`]。
//! 模块面保持最小与正交，不承载任何协议逻辑；TLS 1.3 等协议状态机归上层消费者，
//! 只在这里取原语。原语内化尚未完成：算法与数学能力仍由外部实现闭包提供，不能记为已签收。
//!
//! 安全熵复用 `boost-random` 的宿主安全熵合同：所有取熵路径逐次返回可能失败的
//! [`CryptoError::Entropy`]，不接受固定 seed，不实现外部 RNG trait，不直接触碰平台随机接口。

#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

pub mod aead;
pub mod cipher;
pub mod ecdsa;
pub mod ed25519;
pub mod encoding;
pub mod hash;
pub mod kdf;
pub mod mac;
pub mod rsa;
pub mod secret;
pub mod x25519;

mod aes;
mod entropy;
mod error;

pub use error::{CryptoError, Result};
