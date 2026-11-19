//! `package-registry` 冒烟测试：内存渠道、完整性校验与 tarball 解包边界。

use data_binary_tar::{TarEntryDraft, TarGz, write_tar};
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
fn osv_client_posts_without_auth_and_maps_advisories() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use package_registry::{HttpClient, OsvClient, RetryConfig, osv_ecosystem};

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).is_err() {
                break;
            }
            request.push(byte[0]);
        }
        let body = r#"{"vulns":[{"id":"OSV-1","summary":"demo issue","aliases":["CVE-2026-1"],"database_specific":{"severity":"HIGH"},"affected":[{"ranges":[{"events":[{"fixed":"1.2.3"}]}]}],"references":[{"url":"https://example.invalid/OSV-1"}]}]}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
        let _ = stream.write_all(response.as_bytes());
    });

    let client = OsvClient::with_http(HttpClient::new(RetryConfig::default()).expect("client"))
        .with_endpoint(format!("http://{address}/v1/query"));
    let advisories = client.query("demo", "1.0.0", osv_ecosystem("npm").expect("npm ecosystem")).expect("query");

    assert_eq!(advisories.len(), 1);
    assert_eq!(advisories[0].id.as_deref(), Some("OSV-1"));
    assert_eq!(advisories[0].severity, "HIGH");
    assert_eq!(advisories[0].fixed_version.as_deref(), Some("1.2.3"));
    assert!(osv_ecosystem("valhalla").is_none());

    let head = receiver.recv().expect("request head");
    assert!(head.starts_with("POST /v1/query HTTP/1.1\r\n"), "{head}");
    assert!(!head.to_ascii_lowercase().contains("authorization"), "{head}");
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
