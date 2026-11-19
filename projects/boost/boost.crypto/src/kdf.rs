//! 自有 PBKDF2（RFC 2898 §5.2）密钥派生实现。
//!
//! 只组合本 crate 的 [`crate::mac`] HMAC，不引入外部 KDF crate。`rounds` 必须为正数；
//! 输出长度由调用方切片决定，块序号按大端 4 字节编码。协议侧的盐、迭代数与
//! 口令预处理（例如 SCRAM 的 SASLprep）由上层决定，本模块不预置。

use crate::{
    CryptoError, Result,
    hash::{md5, sha1, sha256},
    mac::hmac,
};

/// SHA-1 的分组长度（HMAC 的 ipad/opad 长度）。
const SHA1_BLOCK: usize = 64;
/// SHA-1 的摘要长度。
const SHA1_LEN: usize = 20;
/// SHA-256 的分组长度。
const SHA256_BLOCK: usize = 64;
/// SHA-256 的摘要长度。
const SHA256_LEN: usize = 32;
/// MD5 的分组长度。
const MD5_BLOCK: usize = 64;
/// MD5 的摘要长度。
const MD5_LEN: usize = 16;

/// 计算 PBKDF2-HMAC-SHA1（RFC 6070 测试序列对应的形态），写入 `out`。
pub fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) -> Result<()> {
    pbkdf2::<{ SHA1_LEN }>(password, salt, rounds, out, |key, data| hmac::<{ SHA1_BLOCK }, { SHA1_LEN }>(key, data, sha1))
}

/// 计算 PBKDF2-HMAC-SHA256，写入 `out`。
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) -> Result<()> {
    pbkdf2::<{ SHA256_LEN }>(password, salt, rounds, out, |key, data| hmac::<{ SHA256_BLOCK }, { SHA256_LEN }>(key, data, sha256))
}

/// 计算 PBKDF2-HMAC-MD5，写入 `out`。
///
/// 只为需要它的既有协议而提供（例如旧式口令派生），新设计优先用 SHA-2。
pub fn pbkdf2_hmac_md5(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) -> Result<()> {
    pbkdf2::<{ MD5_LEN }>(password, salt, rounds, out, |key, data| hmac::<{ MD5_BLOCK }, { MD5_LEN }>(key, data, md5))
}

/// PBKDF2 主体：逐输出块执行 `F(P, S || INT(i), c)` 并拼接截断。
fn pbkdf2<const LEN: usize>(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8], prf: impl Fn(&[u8], &[u8]) -> [u8; LEN]) -> Result<()> {
    if rounds == 0 {
        return Err(CryptoError::InvalidInput);
    }
    let mut offset = 0;
    let mut block_index = 1u32;
    while offset < out.len() {
        let digest = derive_block(password, salt, rounds, block_index, &prf);
        let take = (out.len() - offset).min(LEN);
        out[offset..offset + take].copy_from_slice(&digest[..take]);
        offset += take;
        block_index += 1;
    }
    Ok(())
}

/// 单个输出块：U1 = PRF(P, S || INT(i))，其后 U_c = PRF(P, U_{c-1})，结果为逐轮异或。
fn derive_block<const LEN: usize>(
    password: &[u8],
    salt: &[u8],
    rounds: u32,
    block_index: u32,
    prf: &impl Fn(&[u8], &[u8]) -> [u8; LEN],
) -> [u8; LEN] {
    let mut input = Vec::with_capacity(salt.len() + 4);
    input.extend_from_slice(salt);
    input.extend_from_slice(&block_index.to_be_bytes());

    let mut previous = prf(password, &input);
    let mut result = previous;
    for _ in 1..rounds {
        previous = prf(password, &previous);
        for (byte, &value) in result.iter_mut().zip(&previous) {
            *byte ^= value;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn pbkdf2_sha1_matches_rfc_6070() {
        let mut out = [0u8; 20];
        // RFC 6070 §5.2 测试序列，期望值已与 Node.js `crypto.pbkdf2Sync` 及
        // Python `hashlib.pbkdf2_hmac` 两个独立实现交叉核对。
        pbkdf2_hmac_sha1(b"password", b"salt", 1, &mut out).unwrap();
        assert_eq!(hex(&out), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
        pbkdf2_hmac_sha1(b"password", b"salt", 2, &mut out).unwrap();
        assert_eq!(hex(&out), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957");
        pbkdf2_hmac_sha1(b"password", b"salt", 4096, &mut out).unwrap();
        assert_eq!(hex(&out), "4b007901b765489abead49d926f721d065a429c1");
        // 超过一个 HMAC 分组的口令与盐，输出 25 字节跨块截断。
        let mut long = [0u8; 25];
        pbkdf2_hmac_sha1(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsaltSALT", 4096, &mut long).unwrap();
        assert_eq!(hex(&long), "9f6a91c233c304cb05ec94c1184a6e9cc297cb4afcfa289865");
        // 含 NUL 的口令。
        let mut short = [0u8; 16];
        pbkdf2_hmac_sha1(b"pass\0word", b"sa\0lt", 4096, &mut short).unwrap();
        assert_eq!(hex(&short), "56fa6aa75548099dcc37d7f03425e0c3");
    }

    #[test]
    fn pbkdf2_sha256_matches_independent_implementations() {
        // SHA-256 族向量，期望值由 Node.js 与 Python 两个独立实现交叉核对。
        let mut out = [0u8; 32];
        pbkdf2_hmac_sha256(b"password", b"salt", 1, &mut out).unwrap();
        assert_eq!(hex(&out), "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b");
        pbkdf2_hmac_sha256(b"password", b"salt", 2, &mut out).unwrap();
        assert_eq!(hex(&out), "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43");
        pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut out).unwrap();
        assert_eq!(hex(&out), "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a");
        let mut long = [0u8; 32];
        pbkdf2_hmac_sha256(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsaltSALT", 4096, &mut long).unwrap();
        assert_eq!(hex(&long), "4c196b08b6c5a935e06fcdb29f8a2ce1a4f1cbec1528d9d9dcf52b222c35aaf5");
    }

    #[test]
    fn zero_rounds_is_rejected() {
        let mut out = [0u8; 8];
        assert_eq!(pbkdf2_hmac_sha256(b"p", b"s", 0, &mut out), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn output_spans_multiple_blocks() {
        // 40 字节输出跨两个 SHA-1 块，尾块截断；期望值由独立实现 Node.js
        // `crypto.pbkdf2Sync` 交叉验证生成。
        let mut out = [0u8; 40];
        pbkdf2_hmac_sha1(b"password", b"salt", 2, &mut out).unwrap();
        let mut head = [0u8; 20];
        pbkdf2_hmac_sha1(b"password", b"salt", 2, &mut head).unwrap();
        assert_eq!(&out[..20], &head[..]);
        assert_eq!(hex(&out), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957cae93136266537a8d7bf4b76c51094cc1ae010b1");
    }

    #[test]
    fn md5_variant_matches_independent_implementations() {
        // 期望值由 Node.js `crypto.pbkdf2Sync` 与 Python `hashlib.pbkdf2_hmac` 交叉核对。
        let mut out = [0u8; 16];
        pbkdf2_hmac_md5(b"password", b"salt", 1, &mut out).unwrap();
        assert_eq!(hex(&out), "f31afb6d931392daa5e3130f47f9a9b6");
        pbkdf2_hmac_md5(b"password", b"salt", 4096, &mut out).unwrap();
        assert_eq!(hex(&out), "15001f89b9c29ee6998c520d1a0629e8");
    }
}
