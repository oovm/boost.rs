//! `package-registry` 冒烟测试：内存渠道、完整性校验与 tarball 解包边界。

use acorn_tar::{TarEntryDraft, TarGz, write_tar};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use package_registry::{
    MockRegistry, Package, PublishOptions, Registry, RegistryError, extract_tarball, sha256_hex, verify_sri,
};
use sha2::{Digest, Sha256};

#[test]
fn mock_registry_round_trips_metadata_and_publish() {
    let registry = MockRegistry::new("npm", "https://example.invalid/");
    registry.insert_package(Package { name: "demo".to_string(), version: "1.0.0".to_string(), ..Package::default() });
    registry.insert_token("secret", "publisher");

    let latest = registry.get_package("demo", "latest").expect("latest resolves");
    assert_eq!(latest.version, "1.0.0");
    assert!(matches!(registry.get_package("demo", "9.9.9"), Err(RegistryError::NotFound(_))));
    assert!(registry.verify_token("secret").expect("token check").valid);

    let options = PublishOptions {
        package_name: "demo".to_string(),
        version: "1.1.0".to_string(),
        ..PublishOptions::default()
    };
    let result = registry.publish_package(&options, b"payload").expect("publish recorded");
    assert!(result.success);
    assert_eq!(result.sha256.as_deref(), Some(sha256_hex(b"payload").as_str()));
    assert_eq!(registry.published().len(), 1);
    assert_eq!(registry.get_package_versions("demo").expect("versions").len(), 2);
}

#[test]
fn sri_verification_accepts_matching_digest_and_rejects_mismatch() {
    let data = b"artifact-bytes";
    let integrity = format!("sha256-{}", BASE64.encode(Sha256::digest(data)));
    assert!(verify_sri(data, Some(&integrity)).is_ok());
    assert!(verify_sri(b"other", Some(&integrity)).is_err());
    assert!(verify_sri(data, Some("sha1-whatever")).is_err());
    assert!(verify_sri(data, None).is_ok());
}

#[test]
fn tarball_extraction_strips_package_prefix_and_rejects_escapes() {
    let directory = std::env::temp_dir().join(format!("package-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    let tar = write_tar(vec![
        TarEntryDraft::file("package/README.md", b"# demo".to_vec()),
        TarEntryDraft::file("package/src/lib.rs", b"pub fn demo() {}".to_vec()),
    ])
    .expect("tar written");
    let tarball = TarGz::encode(&tar);

    extract_tarball(&tarball, &directory).expect("tarball extracted");
    assert_eq!(std::fs::read_to_string(directory.join("README.md")).expect("readme"), "# demo");
    assert_eq!(std::fs::read_to_string(directory.join("src/lib.rs")).expect("lib"), "pub fn demo() {}");

    let escaping = write_tar(vec![TarEntryDraft::file("../escape.txt", b"nope".to_vec())]).expect("tar written");
    let error = extract_tarball(&TarGz::encode(&escaping), &directory).expect_err("path escape rejected");
    assert!(error.to_string().contains("escapes the target directory"));

    let _ = std::fs::remove_dir_all(&directory);
}
