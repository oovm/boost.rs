//! `package.cache` 冒烟测试：包索引往返与内容寻址桶的命中 / 失效。

use package_cache::{PackageCache, WorkspaceCache, combined_hash, resolve_marker_root, sanitize_bucket_name};

#[test]
fn package_cache_indexes_installed_directories() {
    let root = std::env::temp_dir().join(format!("package-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");

    let source = root.join("source");
    std::fs::create_dir_all(&source).expect("source dir");
    std::fs::write(source.join("package.von"), "{ name: \"demo\" }").expect("manifest");

    let mut cache = PackageCache::open(root.join("cache")).expect("cache opens");
    cache.add_package("demo", "1.0.0", &source).expect("package added");
    assert!(cache.has_package("demo", "1.0.0"));

    let reopened = PackageCache::open(root.join("cache")).expect("cache reopens");
    let cached = reopened.package_path("demo", "1.0.0").expect("indexed path");
    assert!(cached.join("package.von").is_file());

    let mut cache = reopened;
    cache.clear().expect("cache cleared");
    assert!(!cache.has_package("demo", "1.0.0"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn workspace_cache_round_trips_typed_payloads() {
    let root = std::env::temp_dir().join(format!("package-cache-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let cache = WorkspaceCache::open(&root);
    let key = combined_hash(&["entry", "1"]);
    assert!(cache.get("build", &key, "demo").expect("miss is not an error").is_none());

    cache.put("build", &key, "demo", b"payload").expect("stored");
    assert_eq!(cache.get("build", &key, "demo").expect("hit").as_deref(), Some(&b"payload"[..]));
    // 类型标签不匹配按未命中处理。
    assert!(cache.get("build", &key, "other").expect("type mismatch").is_none());

    cache.invalidate_bucket("build").expect("bucket invalidated");
    assert!(cache.get("build", &key, "demo").expect("miss").is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bucket_names_and_marker_roots_follow_the_cache_contract() {
    assert_eq!(sanitize_bucket_name("build-artifacts here"), "build_artifacts_here");

    let root = std::env::temp_dir().join(format!("package-cache-marker-{}", std::process::id()));
    let nested = root.join("a/b");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&nested).expect("nested dirs");
    std::fs::write(root.join("package.von"), "{ name: \"demo\" }").expect("marker");

    assert_eq!(resolve_marker_root(&nested, "package.von").as_deref(), Some(root.as_path()));
    assert!(resolve_marker_root(&nested, "workspace.von").is_none());

    let _ = std::fs::remove_dir_all(&root);
}
