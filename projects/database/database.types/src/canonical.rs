//! 规范十进制整数文本。
//!
//! 存储格式与 Redis 前端共用同一条规则：可选的 `-`、至少一位十进制数字、
//! 不接受加号、前导零、空白或其他字符，且必须落在 `i64` 范围内。`-0` 不是规范写法。

/// 解析规范十进制整数，非规范写法返回 `None`。
pub fn parse_i64(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() || digits.len() > 19 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let magnitude: u64 = digits.parse().ok()?;
    if negative {
        // `-0` 不是规范写法。
        if magnitude == 0 {
            return None;
        }
        // `-9223372036854775808` 的绝对值比 `i64::MAX` 多一格，仍是合法写法。
        if magnitude == 1 << 63 {
            return Some(i64::MIN);
        }
        if magnitude > 1 << 63 {
            return None;
        }
        Some(-(magnitude as i64))
    }
    else if magnitude <= i64::MAX as u64 {
        Some(magnitude as i64)
    }
    else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::parse_i64;

    #[test]
    fn accepts_canonical_decimal_only() {
        assert_eq!(parse_i64(b"0"), Some(0));
        assert_eq!(parse_i64(b"-1"), Some(-1));
        assert_eq!(parse_i64(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_i64(b"-9223372036854775808"), Some(i64::MIN));
        for rejected in ["", "-", "+1", " 1", "01", "-0", "1 ", "0x10", "1.0", "9223372036854775808"] {
            assert_eq!(parse_i64(rejected.as_bytes()), None, "{rejected} must be rejected");
        }
    }
}
