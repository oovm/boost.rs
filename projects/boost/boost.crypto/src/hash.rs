//! SHA-2 摘要接口。
//!
//! 一次性摘要，返回固定长度数组。流式 transcript 组装属于协议层职责，本模块不预置。

use sha2::{Digest as _, Sha256, Sha384, Sha512};

/// 计算 SHA-256 摘要（32 字节）。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let output = Sha256::digest(data);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(output.as_slice());
    digest
}

/// 计算 SHA-384 摘要（48 字节）。
pub fn sha384(data: &[u8]) -> [u8; 48] {
    let output = Sha384::digest(data);
    let mut digest = [0u8; 48];
    digest.copy_from_slice(output.as_slice());
    digest
}

/// 计算 SHA-512 摘要（64 字节）。
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let output = Sha512::digest(data);
    let mut digest = [0u8; 64];
    digest.copy_from_slice(output.as_slice());
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_lengths() {
        assert_eq!(sha256(b"").len(), 32);
        assert_eq!(sha384(b"").len(), 48);
        assert_eq!(sha512(b"").len(), 64);
    }

    #[test]
    fn deterministic() {
        assert_eq!(sha256(b"abc"), sha256(b"abc"));
        assert_ne!(sha256(b"abc"), sha256(b"abd"));
    }
}
