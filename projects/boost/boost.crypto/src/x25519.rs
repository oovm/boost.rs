//! X25519 密钥交换。
//!
//! 私钥是 32 字节宿主熵，以 [`SecretBytes`](crate::secret::SecretBytes) 承载；派生的共享秘密同样
//! 返回 `SecretBytes`。本模块只做原始 DH 数学，不做协议层的密钥确认。

use crate::{CryptoError, Result, entropy::fill_host, secret::SecretBytes};
use x25519_dalek::{PublicKey, StaticSecret};

/// X25519 私钥长度（字节）。
const SECRET_LEN: usize = 32;

/// 用宿主安全熵生成 X25519 私钥。
///
/// 熵读取失败按 [`CryptoError::Entropy`] 传回，不返回可用私钥。
pub fn generate_secret_key() -> Result<SecretBytes> {
    let mut bytes = [0u8; SECRET_LEN];
    fill_host(&mut bytes)?;
    Ok(SecretBytes::new(bytes.to_vec()))
}

/// 由私钥派生 32 字节公钥（X25519(u, 9)）。
pub fn public_key(secret: &SecretBytes) -> Result<[u8; SECRET_LEN]> {
    let secret = to_static_secret(secret)?;
    Ok(*PublicKey::from(&secret).as_bytes())
}

/// 计算共享秘密 `X25519(secret, peer_public)`。
///
/// 返回的 `SecretBytes` 在 drop 时清零；调用方应在交给 HKDF 之前不复制明文。
pub fn shared_secret(secret: &SecretBytes, peer_public: &[u8; SECRET_LEN]) -> Result<SecretBytes> {
    let secret = to_static_secret(secret)?;
    let peer = PublicKey::from(*peer_public);
    let shared = secret.diffie_hellman(&peer);
    Ok(SecretBytes::new(shared.as_bytes().to_vec()))
}

fn to_static_secret(secret: &SecretBytes) -> Result<StaticSecret> {
    let bytes: [u8; SECRET_LEN] = secret.expose().try_into().map_err(|_| CryptoError::InvalidInput)?;
    Ok(StaticSecret::from(bytes))
}
