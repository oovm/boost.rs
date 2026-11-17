//! HMAC 与 HKDF。
//!
//! 只暴露标准 HKDF（RFC 5869）的 Extract 与 Expand，`info` 为原始字节。TLS 1.3 的
//! Expand-Label 由调用方在外层按 RFC 8446 §7.1 组合 `info` 后传入，本模块不焊协议语义。

use crate::{CryptoError, Result};
use hmac::{Hmac, KeyInit, Mac as _};
use sha2::Sha256;

/// 计算 HMAC-SHA256（32 字节）。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| CryptoError::InvalidInput)?;
    mac.update(data);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_slice());
    Ok(out)
}

/// 以恒定时间比较验证 HMAC-SHA256 标签。
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], tag: &[u8]) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| CryptoError::InvalidInput)?;
    mac.update(data);
    mac.verify_slice(tag).map_err(|_| CryptoError::Authentication)
}

/// HKDF-Extract（RFC 5869 §2.2），返回 PRK（SHA-256 下为 32 字节）。
///
/// `salt` 为 `None` 时使用 HashLen 个零字节，与 RFC 默认盐定义一致。
pub fn hkdf_extract_sha256(salt: Option<&[u8]>, ikm: &[u8]) -> [u8; 32] {
    let (prk, _) = hkdf::Hkdf::<Sha256>::extract(salt, ikm);
    let mut out = [0u8; 32];
    out.copy_from_slice(prk.as_slice());
    out
}

/// HKDF-Expand（RFC 5869 §2.3），把 PRK 与原始 `info` 展开到 `out`。
///
/// `out` 长度超过 HashLen * 255 时返回 [`CryptoError::InvalidInput`]。
pub fn hkdf_expand_sha256(prk: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
    let hkdf = hkdf::Hkdf::<Sha256>::from_prk(prk).map_err(|_| CryptoError::InvalidInput)?;
    hkdf.expand(info, out).map_err(|_| CryptoError::InvalidInput)
}

/// HKDF 一步式：Extract 后直接 Expand 到 `out`。
pub fn hkdf_sha256(salt: Option<&[u8]>, ikm: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
    let hkdf = hkdf::Hkdf::<Sha256>::new(salt, ikm);
    hkdf.expand(info, out).map_err(|_| CryptoError::InvalidInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_matching_tag() {
        let tag = hmac_sha256(b"key", b"data").expect("hmac");
        hmac_sha256_verify(b"key", b"data", &tag).expect("valid tag");
    }

    #[test]
    fn verify_rejects_tampered_tag() {
        let mut tag = hmac_sha256(b"key", b"data").expect("hmac");
        tag[0] ^= 0x01;
        assert_eq!(hmac_sha256_verify(b"key", b"data", &tag), Err(CryptoError::Authentication));
    }

    #[test]
    fn expand_overlong_output_fails() {
        let prk = hkdf_extract_sha256(None, b"ikm");
        let mut out = vec![0u8; 32 * 255 + 1];
        assert_eq!(hkdf_expand_sha256(&prk, b"info", &mut out), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn extract_with_none_salt_matches_zero_salt() {
        let zeros = [0u8; 32];
        assert_eq!(hkdf_extract_sha256(None, b"ikm"), hkdf_extract_sha256(Some(&zeros), b"ikm"));
    }
}
