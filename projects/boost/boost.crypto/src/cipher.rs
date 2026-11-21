//! 对称分组密码模式。
//!
//! 这里只提供调用方明确要求「原始模式」时需要的块解密：不做填充剥离、不做认证。
//! 需要认证的路径使用 [`crate::aead`]；需要填充的路径由上层按协议规定处理。
//! AES 分组实现位于 [`crate::aes`]，本模块只负责 CBC 的链式 XOR。

use crate::aes::AesKey;
use crate::{CryptoError, Result};

/// 以 AES-128-CBC 解密，输入长度必须是 16 的整数倍。
///
/// 不剥离 PKCS#7 填充；调用方拿到的是原始明文块。
pub fn aes_128_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(CryptoError::InvalidInput);
    }
    decrypt(key, iv, ciphertext)
}

/// 以 AES-256-CBC 解密，输入长度必须是 16 的整数倍。
///
/// 不剥离 PKCS#7 填充；调用方拿到的是原始明文块。
pub fn aes_256_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 32 {
        return Err(CryptoError::InvalidInput);
    }
    decrypt(key, iv, ciphertext)
}

fn decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if iv.len() != 16 || ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(CryptoError::InvalidInput);
    }
    let cipher = AesKey::new(key).ok_or(CryptoError::InvalidInput)?;
    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);
    let mut plaintext = Vec::with_capacity(ciphertext.len());
    for chunk in ciphertext.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        // CBC 解密用当前密文块作为下一块的链值，所以先留副本再解块。
        let chained = block;
        cipher.decrypt_block(&mut block);
        for (byte, link) in block.iter_mut().zip(previous.iter()) {
            *byte ^= link;
        }
        previous = chained;
        plaintext.extend_from_slice(&block);
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::{aes_128_cbc_decrypt, aes_256_cbc_decrypt};

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap()).collect()
    }

    #[test]
    fn decrypts_nist_aes_128_cbc_vector() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let ciphertext = hex("7649abac8119b246cee98e9b12e9197d");
        let plaintext = hex("6bc1bee22e409f96e93d7e117393172a");
        assert_eq!(aes_128_cbc_decrypt(&key, &iv, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn decrypts_nist_aes_256_cbc_vector() {
        let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let ciphertext = hex("f58c4c04d6e5f1ba779eabfb5f7bfbd6");
        let plaintext = hex("6bc1bee22e409f96e93d7e117393172a");
        assert_eq!(aes_256_cbc_decrypt(&key, &iv, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn decrypts_multi_block_aes_128_cbc_vector() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let ciphertext = hex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2");
        let plaintext =
            hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        assert_eq!(aes_128_cbc_decrypt(&key, &iv, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn rejects_malformed_inputs() {
        assert!(aes_128_cbc_decrypt(&[0; 15], &[0; 16], &[0; 16]).is_err());
        assert!(aes_256_cbc_decrypt(&[0; 32], &[0; 15], &[0; 16]).is_err());
        assert!(aes_256_cbc_decrypt(&[0; 32], &[0; 16], &[0; 15]).is_err());
        assert!(aes_256_cbc_decrypt(&[0; 32], &[0; 16], &[]).is_err());
    }
}
