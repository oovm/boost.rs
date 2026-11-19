//! 字符串准备（StringPrep）家族：RFC 4013 SASLprep。
//!
//! 本 crate 拥有 SASL 口令预处理的唯一实现：表 B.1/C.1.2 映射、NFKC 规范化、
//! RFC 3454 附录 C 禁止集合与 §6 双向检查，顺序固定为映射 → 规范化 → 禁止 → 双向。
//! NFKC 由 `unicode-normalization` 提供（内化未完成，不能记为已签收）；
//! 表内容由 `tools/generate_saslprep_tables.py` 从 CPython `stringprep` 生成。
//! 协议侧（SCRAM 等）只在这里取结果，不再各自维护口令规范化副本。

#![warn(missing_docs)]

mod tables;

use unicode_normalization::UnicodeNormalization;

/// SASLprep 失败的两类原因，对应 RFC 4013 §2.3 的禁止码点与 RFC 3454 §6 的双向检查。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaslprepError {
    /// 规范化后的字符串含 RFC 4013 §2.3 禁止的码点。
    Prohibited,
    /// 含 RandALCat 码点时不满足 RFC 3454 §6：混入 LCat，或首尾不是 RandALCat。
    Bidi,
}

/// 表查询：区间按码点升序且不重叠，二分定位。
fn in_ranges(ranges: &[(u32, u32)], code: u32) -> bool {
    ranges
        .binary_search_by(|&(start, end)| {
            if end < code {
                core::cmp::Ordering::Less
            }
            else if start > code {
                core::cmp::Ordering::Greater
            }
            else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// RFC 4013 SASLprep：对口令做映射、NFKC 规范化、禁止检查与双向检查。
///
/// 输入必须是良构 UTF-8；孤立代理对无法进入本函数，表中的代理区间因此只对
/// 由其它来源转码而来的字节序列保持完整语义。
pub fn saslprep(input: &str) -> Result<String, SaslprepError> {
    let mut mapped = String::with_capacity(input.len());
    for character in input.chars() {
        let code = character as u32;
        if in_ranges(tables::MAPPED_TO_NOTHING, code) {
            continue;
        }
        if in_ranges(tables::MAPPED_TO_SPACE, code) {
            mapped.push(' ');
            continue;
        }
        mapped.push(character);
    }

    let normalized: String = mapped.nfkc().collect();
    if normalized.chars().any(|c| in_ranges(tables::PROHIBITED, c as u32)) {
        return Err(SaslprepError::Prohibited);
    }

    // RFC 3454 §6：出现 RandALCat 时，禁止混入 LCat，且首尾字符必须是 RandALCat。
    if normalized.chars().any(|c| in_ranges(tables::RAND_ALCAT, c as u32)) {
        if normalized.chars().any(|c| in_ranges(tables::L_CAT, c as u32)) {
            return Err(SaslprepError::Bidi);
        }
        let boundary_is_randal =
            [normalized.chars().next(), normalized.chars().next_back()].into_iter().flatten().all(|c| in_ranges(tables::RAND_ALCAT, c as u32));
        if !boundary_is_randal {
            return Err(SaslprepError::Bidi);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passwords_pass_through() {
        assert_eq!(saslprep("").unwrap(), "");
        assert_eq!(saslprep("x").unwrap(), "x");
        assert_eq!(saslprep("password123!~<>").unwrap(), "password123!~<>");
        // 前后空格不属于映射集合，SASLprep 不剥空白。
        assert_eq!(saslprep("  x  ").unwrap(), "  x  ");
    }

    #[test]
    fn table_b1_characters_are_removed() {
        assert_eq!(saslprep("a\u{00AD}b").unwrap(), "ab");
        assert_eq!(saslprep("a\u{200B}b").unwrap(), "ab");
        assert_eq!(saslprep("a\u{200C}b").unwrap(), "ab");
        assert_eq!(saslprep("a\u{2060}b").unwrap(), "ab");
        assert_eq!(saslprep("a\u{FEFF}b").unwrap(), "ab");
    }

    #[test]
    fn non_ascii_spaces_map_to_space() {
        assert_eq!(saslprep("a\u{00A0}b").unwrap(), "a b");
        assert_eq!(saslprep("a\u{2000}b").unwrap(), "a b");
        assert_eq!(saslprep("a\u{3000}b").unwrap(), "a b");
        assert_eq!(saslprep("a\u{200A}b").unwrap(), "a b");
    }

    #[test]
    fn nfkc_normalization_applies_after_mapping() {
        // U+FF21 全角 A → "A"；U+2168 罗马数字 IX → "IX"；U+00AA → "a"。
        assert_eq!(saslprep("\u{FF21}").unwrap(), "A");
        assert_eq!(saslprep("\u{2168}").unwrap(), "IX");
        assert_eq!(saslprep("\u{00AA}").unwrap(), "a");
        // U+2126 OHM 与 U+03A9 GREEK CAPITAL OMEGA 规范化后相等。
        assert_eq!(saslprep("\u{2126}").unwrap(), saslprep("\u{03A9}").unwrap());
    }

    #[test]
    fn prohibited_code_points_are_rejected() {
        // 每个码点都属于 RFC 3454 附录 C 的某个禁止表（CPython `stringprep` 交叉核对）。
        // 表按 RFC 3454 冻结于 Unicode 3.2：晚近新增的 Cf（如 U+0605、U+08E2）不在其中，
        // 与仓内既有 TS 实现（经 mongod 8.0.9 验证）保持同一口径。
        for text in [
            "a\u{0000}b",  // C.2.1
            "a\u{001F}b",  // C.2.1
            "a\u{0080}b",  // C.2.1
            "a\u{06DD}b",  // C.2.2
            "a\u{070F}b",  // C.2.2
            "a\u{180E}b",  // C.2.2（Unicode 3.2 的 Cf）
            "a\u{200E}b",  // C.2.2
            "a\u{2028}b",  // C.2.2
            "a\u{202A}b",  // C.2.2
            "a\u{206F}b",  // C.2.2
            "a\u{2FF0}b",  // C.7
            "a\u{E000}b",  // C.3
            "a\u{FDD0}b",  // C.4
            "a\u{FFF9}b",  // C.4
            "a\u{FFFE}b",  // C.4
            "a\u{1D173}b", // C.8
            "a\u{E0001}b", // C.9
            "a\u{E0021}b", // C.9
            "a\u{F0000}b", // C.3
        ] {
            assert_eq!(saslprep(text), Err(SaslprepError::Prohibited), "unexpected acceptance for {text:?}");
        }
    }

    #[test]
    fn bidi_rules_match_reference() {
        // 希伯来文（RandALCat）单独出现合法。
        assert_eq!(saslprep("\u{05D0}\u{05D1}"), Ok("\u{05D0}\u{05D1}".into()));
        // RandALCat 混入 LCat 非法。
        assert_eq!(saslprep("\u{05D0}x"), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("x\u{05D0}"), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("\u{05D0}\u{003F}"), Err(SaslprepError::Bidi));
        // 首尾 RandALCat、中间允许非 LCat 的非 RandALCat（问号是 ON，不是 L）。
        assert_eq!(saslprep("\u{05D0}\u{003F}\u{05D1}"), Ok("\u{05D0}\u{003F}\u{05D1}".into()));
        // 阿拉伯文同样是 RandALCat。
        assert!(saslprep("\u{0627}\u{0644}\u{0639}").is_ok());
    }

    #[test]
    fn mapping_happens_before_prohibition() {
        // U+FEFF 同时出现在 B.1 与 C.2.1：映射先行，结果合法且被删除。
        assert_eq!(saslprep("\u{FEFF}"), Ok(String::new()));
        // U+200B 同理。
        assert_eq!(saslprep("\u{200B}"), Ok(String::new()));
    }
}
