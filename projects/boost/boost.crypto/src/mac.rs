//! 自有 HMAC 与 HKDF 实现。
//!
//! 只用本 crate 的 [`crate::hash`] 摘要，遵循 RFC 2104 与 RFC 5869。`info` 为原始字节：
//! TLS 1.3 的 Expand-Label 由调用方在外层按 RFC 8446 §7.1 组合后传入，本模块不焊协议语义。

use crate::{
    CryptoError, Result,
    hash::{sha1, sha256, sha384},
};

/// SHA-1 的分组长度（HMAC 的 ipad/opad 长度）。
const SHA1_BLOCK: usize = 64;
/// SHA-1 的摘要长度。
const SHA1_LEN: usize = 20;

/// SHA-256 的分组长度（HMAC 的 ipad/opad 长度）。
const SHA256_BLOCK: usize = 64;
/// SHA-256 的摘要长度。
const SHA256_LEN: usize = 32;

/// SHA-384 的分组长度。
const SHA384_BLOCK: usize = 128;
/// SHA-384 的摘要长度。
const SHA384_LEN: usize = 48;

/// 计算 HMAC-SHA1（20 字节）。
///
/// 只为需要它的既有协议而提供（如部分云厂商的对象存储签名），新设计优先用 SHA-256。
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> Result<[u8; 20]> {
    Ok(hmac::<{ SHA1_BLOCK }, { SHA1_LEN }>(key, data, sha1))
}

/// 计算 HMAC-SHA256（32 字节）。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<[u8; 32]> {
    Ok(hmac::<{ SHA256_BLOCK }, { SHA256_LEN }>(key, data, sha256))
}

/// 计算 HMAC-SHA384（48 字节）。
pub fn hmac_sha384(key: &[u8], data: &[u8]) -> Result<[u8; 48]> {
    Ok(hmac::<{ SHA384_BLOCK }, { SHA384_LEN }>(key, data, sha384))
}

/// HMAC 本身对任意长度 key 都有定义，内部实现不产生失败。
pub(crate) fn hmac<const BLOCK: usize, const LEN: usize>(key: &[u8], data: &[u8], hash: fn(&[u8]) -> [u8; LEN]) -> [u8; LEN] {
    let mut normalized = [0u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..LEN].copy_from_slice(&hash(key));
    }
    else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend_from_slice(&xor_pad::<BLOCK>(&normalized, 0x36));
    inner.extend_from_slice(data);
    let inner_digest = hash(&inner);

    let mut outer = Vec::with_capacity(BLOCK + LEN);
    outer.extend_from_slice(&xor_pad::<BLOCK>(&normalized, 0x5c));
    outer.extend_from_slice(&inner_digest);
    hash(&outer)
}

/// 以恒定时间比较验证 HMAC-SHA256 标签。
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], tag: &[u8]) -> Result<()> {
    let expected = hmac_sha256(key, data)?;
    if constant_time_eq(&expected, tag) { Ok(()) } else { Err(CryptoError::Authentication) }
}

/// HKDF-Extract（RFC 5869 §2.2），返回 PRK（SHA-256 下为 32 字节）。
///
/// `salt` 为 `None` 时使用 HashLen 个零字节，与 RFC 默认盐定义一致。
pub fn hkdf_extract_sha256(salt: Option<&[u8]>, ikm: &[u8]) -> [u8; 32] {
    let zeros = [0u8; SHA256_LEN];
    let salt = salt.unwrap_or(&zeros);
    hmac::<{ SHA256_BLOCK }, { SHA256_LEN }>(salt, ikm, sha256)
}

/// HKDF-Expand（RFC 5869 §2.3），把 PRK 与原始 `info` 展开到 `out`。
///
/// `out` 长度超过 HashLen * 255 时返回 [`CryptoError::InvalidInput`]。
pub fn hkdf_expand_sha256(prk: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
    if out.len() > SHA256_LEN * 255 {
        return Err(CryptoError::InvalidInput);
    }
    // RFC 5869: T(0) 为空串，T(i) = HMAC(PRK, T(i-1) ‖ info ‖ i)。
    let mut previous: Vec<u8> = Vec::new();
    let mut filled = 0usize;
    let mut counter = 1u8;
    while filled < out.len() {
        let mut input = Vec::with_capacity(previous.len() + info.len() + 1);
        input.extend_from_slice(&previous);
        input.extend_from_slice(info);
        input.push(counter);
        let block = hmac::<{ SHA256_BLOCK }, { SHA256_LEN }>(prk, &input, sha256);
        let take = (out.len() - filled).min(SHA256_LEN);
        out[filled..filled + take].copy_from_slice(&block[..take]);
        filled += take;
        previous = block.to_vec();
        counter = counter.checked_add(1).ok_or(CryptoError::InvalidInput)?;
    }
    Ok(())
}

/// HKDF 一步式：Extract 后直接 Expand 到 `out`。
pub fn hkdf_sha256(salt: Option<&[u8]>, ikm: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
    let prk = hkdf_extract_sha256(salt, ikm);
    hkdf_expand_sha256(&prk, info, out)
}

pub(crate) fn xor_pad<const BLOCK: usize>(block: &[u8; BLOCK], byte: u8) -> [u8; BLOCK] {
    let mut out = [0u8; BLOCK];
    for index in 0..BLOCK {
        out[index] = block[index] ^ byte;
    }
    out
}

/// 长度不同直接判否，长度相同则按位累积差异后一次判断，避免提前返回泄露位置。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn hmac_rfc4231_case_1() {
        // key = 0x0b * 20, data = "Hi There"
        let key = [0x0bu8; 20];
        assert_eq!(hex(&hmac_sha256(&key, b"Hi There").expect("hmac")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
    }

    #[test]
    fn hmac_rfc4231_case_2() {
        // key = "Jefe", data = "what do ya want for nothing?"
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?").expect("hmac")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_long_key_is_hashed_first() {
        // 超过分组长度的 key 先取摘要，结果必须与显式两步一致。
        let long_key = vec![0xaau8; SHA256_BLOCK + 1];
        let direct = hmac_sha256(&long_key, b"data").expect("hmac");
        let hashed = sha256(&long_key);
        let two_step = hmac_sha256(&hashed, b"data").expect("hmac");
        assert_eq!(direct, two_step);
    }

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
        assert_eq!(hmac_sha256_verify(b"key", b"data", &tag[..16]), Err(CryptoError::Authentication));
    }

    #[test]
    fn hkdf_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let prk = hkdf_extract_sha256(Some(&salt), &ikm);
        assert_eq!(hex(&prk), "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5");
        let mut out = [0u8; 42];
        hkdf_expand_sha256(&prk, &info, &mut out).expect("expand");
        assert_eq!(hex(&out), "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865");
    }

    #[test]
    fn expand_overlong_output_fails() {
        let prk = hkdf_extract_sha256(None, b"ikm");
        let mut out = vec![0u8; SHA256_LEN * 255 + 1];
        assert_eq!(hkdf_expand_sha256(&prk, b"info", &mut out), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn extract_with_none_salt_matches_zero_salt() {
        let zeros = [0u8; SHA256_LEN];
        assert_eq!(hkdf_extract_sha256(None, b"ikm"), hkdf_extract_sha256(Some(&zeros), b"ikm"));
    }

    #[test]
    fn expand_across_multiple_blocks_is_stable() {
        let prk = hkdf_extract_sha256(Some(b"salt"), b"ikm");
        let mut whole = [0u8; 96];
        hkdf_expand_sha256(&prk, b"info", &mut whole).expect("expand");
        let mut first = [0u8; 32];
        hkdf_expand_sha256(&prk, b"info", &mut first).expect("expand");
        assert_eq!(&whole[..32], &first[..]);
    }
}
