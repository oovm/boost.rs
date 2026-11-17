//! AES-GCM 认证加密。
//!
//! seal 输出为「密文 || 16 字节认证标签」拼接形态，open 接受同一形态。nonce 必须由调用方给出
//! 并保证在密钥生命周期内唯一，库不生成 nonce，也不提供重用防护模式。

use crate::{CryptoError, Result};
use aes_gcm::{
    Aes128Gcm, Aes256Gcm,
    aead::{Aead, KeyInit, Payload, generic_array::GenericArray},
};

/// AES-GCM 的 nonce 长度（96 bit）。
const NONCE_LEN: usize = 12;

fn seal<C: KeyInit + Aead>(cipher_key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return Err(CryptoError::InvalidInput);
    }
    let cipher = C::new_from_slice(cipher_key).map_err(|_| CryptoError::InvalidInput)?;
    let nonce = GenericArray::from_slice(nonce);
    cipher.encrypt(nonce, Payload { msg: plaintext, aad }).map_err(|_| CryptoError::Authentication)
}

fn open<C: KeyInit + Aead>(cipher_key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return Err(CryptoError::InvalidInput);
    }
    let cipher = C::new_from_slice(cipher_key).map_err(|_| CryptoError::InvalidInput)?;
    let nonce = GenericArray::from_slice(nonce);
    cipher.decrypt(nonce, Payload { msg: ciphertext, aad }).map_err(|_| CryptoError::Authentication)
}

/// 以 AES-128-GCM 加密。`key` 必须为 16 字节，`nonce` 必须为 12 字节。
pub fn aes_128_gcm_seal(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    seal::<Aes128Gcm>(key, nonce, aad, plaintext)
}

/// 以 AES-128-GCM 解密并验证标签。失败统一返回 [`CryptoError::Authentication`]。
pub fn aes_128_gcm_open(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    open::<Aes128Gcm>(key, nonce, aad, ciphertext)
}

/// 以 AES-256-GCM 加密。`key` 必须为 32 字节，`nonce` 必须为 12 字节。
pub fn aes_256_gcm_seal(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    seal::<Aes256Gcm>(key, nonce, aad, plaintext)
}

/// 以 AES-256-GCM 解密并验证标签。失败统一返回 [`CryptoError::Authentication`]。
pub fn aes_256_gcm_open(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    open::<Aes256Gcm>(key, nonce, aad, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_128_and_256() {
        let key128 = [0x42u8; 16];
        let key256 = [0x42u8; 32];
        let nonce = [0x07u8; NONCE_LEN];
        let plaintext = b"boost crypto aead";
        let sealed = aes_128_gcm_seal(&key128, &nonce, b"aad", plaintext).expect("seal");
        assert_eq!(aes_128_gcm_open(&key128, &nonce, b"aad", &sealed).expect("open"), plaintext);
        let sealed = aes_256_gcm_seal(&key256, &nonce, b"", plaintext).expect("seal");
        assert_eq!(aes_256_gcm_open(&key256, &nonce, b"", &sealed).expect("open"), plaintext);
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let key = [0x42u8; 16];
        let nonce = [0x07u8; NONCE_LEN];
        let mut sealed = aes_128_gcm_seal(&key, &nonce, b"aad", b"data").expect("seal");
        sealed[0] ^= 0x01;
        assert_eq!(aes_128_gcm_open(&key, &nonce, b"aad", &sealed), Err(CryptoError::Authentication));
    }

    #[test]
    fn wrong_nonce_length_rejected() {
        assert_eq!(aes_128_gcm_seal(&[0u8; 16], &[0u8; 8], b"", b""), Err(CryptoError::InvalidInput));
    }
}
