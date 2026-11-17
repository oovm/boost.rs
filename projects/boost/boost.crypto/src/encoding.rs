//! base64 编码。
//!
//! 提供标准填充与无填充两种形态的编解码，供证书、密钥与协议字段在文本边界使用。

use crate::{CryptoError, Result};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};

/// 标准 base64 编码（带 `=` 填充）。
pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// 无填充 base64 编码。
pub fn base64_encode_no_pad(bytes: &[u8]) -> String {
    STANDARD_NO_PAD.encode(bytes)
}

/// 解码标准 base64（接受带填充输入）。
pub fn base64_decode(text: &str) -> Result<Vec<u8>> {
    STANDARD.decode(text).map_err(|_| CryptoError::InvalidInput)
}

/// 解码无填充 base64（接受不带填充输入）。
pub fn base64_decode_no_pad(text: &str) -> Result<Vec<u8>> {
    STANDARD_NO_PAD.decode(text).map_err(|_| CryptoError::InvalidInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode_no_pad(b"f"), "Zg");
        assert_eq!(base64_encode_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64_decode("Zm9vYg==").expect("decode"), b"foob");
        assert_eq!(base64_decode_no_pad("Zm9vYg").expect("decode"), b"foob");
    }

    #[test]
    fn roundtrip_all_byte_values() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&bytes)).expect("decode"), bytes);
        assert_eq!(base64_decode_no_pad(&base64_encode_no_pad(&bytes)).expect("decode"), bytes);
    }

    #[test]
    fn invalid_input_rejected() {
        assert_eq!(base64_decode("Zm9v*"), Err(CryptoError::InvalidInput));
    }
}
