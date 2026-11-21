//! Unity 版本文本解析与比较。
//!
//! SerializedFile 元数据里的版本字符串形如 `2020.3.41f1`，中国版会多出后缀
//! （例如 `2022.3.14f1c1`）。比较时只看语义化部分：主/次/修订号、发布类型与
//! 发布序号，后缀不参与排序。

use crate::{UnityError, UnityVersion};

/// 解析 Unity 版本文本。
pub(crate) fn parse_unity_version(text: &str) -> Result<UnityVersion, UnityError> {
    let text = text.trim();
    let mut parts = text.split('.');
    let major =
        parts.next().and_then(|value| value.parse::<u32>().ok()).ok_or_else(|| UnityError::Parse(format!("invalid unity version `{text}`")))?;
    let minor =
        parts.next().and_then(|value| value.parse::<u32>().ok()).ok_or_else(|| UnityError::Parse(format!("invalid unity version `{text}`")))?;
    let rest = parts.next().ok_or_else(|| UnityError::Parse(format!("invalid unity version `{text}`")))?;

    let kind_index = rest
        .char_indices()
        .find_map(|(index, ch)| ch.is_ascii_alphabetic().then_some(index))
        .ok_or_else(|| UnityError::Parse(format!("invalid unity version `{text}`")))?;
    let build = rest[..kind_index].parse::<u32>().map_err(|_| UnityError::Parse(format!("invalid unity version `{text}`")))?;
    let kind = rest[kind_index..].chars().next().ok_or_else(|| UnityError::Parse(format!("invalid unity version `{text}`")))?;

    let tail = &rest[kind_index + kind.len_utf8()..];
    let digit_end = tail.char_indices().find_map(|(index, ch)| (!ch.is_ascii_digit()).then_some(index)).unwrap_or(tail.len());
    let revision = if digit_end == 0 {
        0
    }
    else {
        tail[..digit_end].parse::<u32>().map_err(|_| UnityError::Parse(format!("invalid unity version `{text}`")))?
    };
    let suffix = tail[digit_end..].to_string();

    Ok(UnityVersion { major, minor, build, kind, revision, suffix, text: text.to_string() })
}

/// 由 TPK 存储的字段构造版本号。
pub(crate) fn unity_version_from_parts(major: u32, minor: u32, build: u32, kind_code: u8, revision: u32) -> UnityVersion {
    let kind = match kind_code {
        0 => 'a',
        1 => 'b',
        2 => 'c',
        3 => 'f',
        4 => 'p',
        5 => 'e',
        _ => 'f',
    };
    let text = format!("{major}.{minor}.{build}{kind}{revision}");
    UnityVersion { major, minor, build, kind, revision, suffix: String::new(), text }
}

fn kind_rank(kind: char) -> u8 {
    match kind {
        'a' => 0,
        'b' => 1,
        'c' => 2,
        'f' => 3,
        'p' => 4,
        'e' => 5,
        _ => 3,
    }
}

fn version_key(version: &UnityVersion) -> (u32, u32, u32, u8, u32) {
    (version.major, version.minor, version.build, kind_rank(version.kind), version.revision)
}

/// `target` 是否不早于 `candidate`。
pub(crate) fn version_at_least(target: &UnityVersion, candidate: &UnityVersion) -> bool {
    version_key(target) >= version_key(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_version() {
        let version = parse_unity_version("2020.3.41f1").unwrap();
        assert_eq!(version.major, 2020);
        assert_eq!(version.minor, 3);
        assert_eq!(version.build, 41);
        assert_eq!(version.kind, 'f');
        assert_eq!(version.revision, 1);
        assert_eq!(version.suffix, "");
        assert_eq!(version.text, "2020.3.41f1");
        assert_eq!(version.to_string(), "2020.3.41f1");
    }

    #[test]
    fn parses_chinese_version_suffix() {
        let version = parse_unity_version("2022.3.14f1c1").unwrap();
        assert_eq!(version.major, 2022);
        assert_eq!(version.minor, 3);
        assert_eq!(version.build, 14);
        assert_eq!(version.kind, 'f');
        assert_eq!(version.revision, 1);
        assert_eq!(version.suffix, "c1");
        assert_eq!(version.text, "2022.3.14f1c1");
    }

    #[test]
    fn rejects_invalid_version() {
        assert!(parse_unity_version("not-a-version").is_err());
        assert!(parse_unity_version("2020.3").is_err());
        assert!(parse_unity_version("2020.x.41f1").is_err());
    }

    #[test]
    fn compares_semantic_parts() {
        let old = parse_unity_version("2020.3.41f1").unwrap();
        let new = parse_unity_version("2020.3.42f1").unwrap();
        assert!(version_at_least(&new, &old));
        assert!(!version_at_least(&old, &new));
        assert!(version_at_least(&old, &old));
    }
}
