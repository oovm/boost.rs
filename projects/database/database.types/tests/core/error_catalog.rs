//! 保证 `Error::code()` 在本 crate 内唯一，且落在 `database.` 命名空间。
//!
//! 产品壳的公开错误目录由各产品仓自行维护；底库测试不钉产品路径。

use std::{collections::BTreeSet, fs};

fn engine_codes() -> BTreeSet<String> {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).expect("crate source is readable");
    source
        .lines()
        .filter(|line| line.contains("=> \"database."))
        .filter_map(|line| {
            let start = line.find("=> \"")? + 4;
            let rest = &line[start..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

#[test]
fn engine_codes_are_unique_and_unbranded() {
    let codes = engine_codes();
    assert_eq!(codes.len(), 34, "code count changed: {codes:?}");
    for code in &codes {
        assert!(code.starts_with("database."), "`{code}` must stay inside the `database.` namespace");
        assert!(!code.starts_with("yydb."), "`{code}` must not use product-branded prefix");
        assert!(!code.starts_with("yyds."), "`{code}` must not use product-branded prefix");
    }
}

#[test]
fn shard_failures_keep_their_own_diagnostic_category() {
    use database_types::{Error, cluster::Error as ClusterError};

    let corrupt = Error::from(ClusterError::Corrupt("payload is unreadable"));
    assert_eq!(corrupt.code(), "database.shard.corrupt");
    assert_eq!(corrupt.operation(), "io");
    assert!(!corrupt.retryable());

    let conflict = Error::from(ClusterError::CasConflict { key: "ns:k".to_owned() });
    assert_eq!(conflict.code(), "database.shard.cas.conflict");
    assert_eq!(conflict.operation(), "cas");
    assert_eq!(conflict.params().and_then(|pairs| pairs.into_iter().find(|(name, _)| *name == "key")).map(|(_, value)| value), Some("ns:k".to_owned()));

    let io = Error::from(ClusterError::Io(std::io::Error::other("disk gone")));
    assert_eq!(io.code(), "database.shard.io");
    assert!(io.retryable());

    assert_ne!(corrupt.code(), "database.protocol", "storage failures must not collapse into the protocol category");
}
