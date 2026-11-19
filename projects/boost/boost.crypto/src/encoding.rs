//! base64 与十六进制编码。
//!
//! 提供标准填充与无填充两种 base64 形态，以及小写十六进制编解码，供证书、密钥、
//! 签名与协议字段在文本边界使用。

use crate::{CryptoError, Result};
use data_encoding::representation::{Base64, Padding};

/// 小写十六进制字符表。
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// 小写十六进制编码。
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(LOWER_HEX[(byte >> 4) as usize] as char);
        out.push(LOWER_HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 解码十六进制，大小写均可。
///
/// 输入长度为奇数或含非十六进制字符时返回 [`CryptoError::InvalidInput`]。
pub fn hex_decode(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(CryptoError::InvalidInput);
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_digit(bytes[index])?;
        let low = hex_digit(bytes[index + 1])?;
        out.push(high << 4 | low);
        index += 2;
    }
    Ok(out)
}

/// 无填充的 URL-safe base64 编码（`+`→`-`、`/`→`_`，省略 `=`）。
///
/// 用于 JOSE、ACME 与各类把摘要塞进 URL 或 JSON 字符串的协议。
pub fn base64url_encode(bytes: &[u8]) -> String {
    Base64::url_safe_no_padding().encode_text(bytes)
}

/// 解码无填充的 URL-safe base64，同时接受带填充输入。
pub fn base64url_decode(text: &str) -> Result<Vec<u8>> {
    Base64::url_safe_no_padding().decode_canonical(text.trim_end_matches('=')).map_err(|_| CryptoError::InvalidInput)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(CryptoError::InvalidInput),
    }
}

/// 标准 base64 编码（带 `=` 填充）。
pub fn base64_encode(bytes: &[u8]) -> String {
    Base64::standard_strict().encode_text(bytes)
}

/// 无填充 base64 编码。
pub fn base64_encode_no_pad(bytes: &[u8]) -> String {
    Base64::standard_strict().with_padding(Padding::Forbidden).encode_text(bytes)
}

/// 解码标准 base64（接受带填充输入）。
pub fn base64_decode(text: &str) -> Result<Vec<u8>> {
    Base64::standard_strict().decode_canonical(text).map_err(|_| CryptoError::InvalidInput)
}

/// 解码无填充 base64（接受不带填充输入）。
pub fn base64_decode_no_pad(text: &str) -> Result<Vec<u8>> {
    Base64::standard_strict().with_padding(Padding::Forbidden).decode_canonical(text).map_err(|_| CryptoError::InvalidInput)
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

    #[test]
    fn hex_roundtrip_and_case() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_decode("000fa5ff").expect("decode"), vec![0x00, 0x0f, 0xa5, 0xff]);
        assert_eq!(hex_decode("000FA5FF").expect("decode"), vec![0x00, 0x0f, 0xa5, 0xff]);
    }

    #[test]
    fn hex_rejects_odd_length_and_bad_digits() {
        assert_eq!(hex_decode("abc"), Err(CryptoError::InvalidInput));
        assert_eq!(hex_decode("zz"), Err(CryptoError::InvalidInput));
        assert_eq!(hex_decode("0g"), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        // 这三组字节在标准 base64 里分别产生 `+`、`/` 与 `=`，URL-safe 形态必须全部避开。
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(&[]), "");
        assert!(!base64url_encode(&[0xff, 0xfe, 0xfd]).contains(['+', '/', '=']));
    }

    #[test]
    fn base64url_roundtrip_accepts_padding() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(base64url_decode(&base64url_encode(&bytes)).expect("decode"), bytes);
        assert_eq!(base64url_decode("Zm9vYg==").expect("decode"), b"foob");
        assert_eq!(base64url_decode("-_8").expect("decode"), vec![0xfb, 0xff]);
    }
}
