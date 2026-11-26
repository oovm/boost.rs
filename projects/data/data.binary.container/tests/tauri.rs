//! Tauri v2 asset-table traversal and extraction.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use data_binary_container::tauri::{TauriAssetsOptions, analyze, scan_assets};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("data-container-{}-{id}-{tag}", std::process::id()))
}

/// One embedded asset vector: key, plaintext and the compressed hex literal.
struct AssetVector {
    key: &'static str,
    plaintext: &'static [u8],
    compressed: &'static str,
}

/// Decodes a hex-encoded compressed fixture.
fn compressed(value: &str) -> Vec<u8> {
    assert!(value.len() % 2 == 0, "odd number of hex digits");
    (0..value.len()).step_by(2).map(|index| u8::from_str_radix(&value[index..index + 2], 16).expect("valid hex")).collect()
}

/// Embedded assets produced once with Node.js 24 `zlib.brotliCompressSync`
/// (quality 9, default 22-bit window), the settings Tauri v2 codegen uses for
/// every asset. `INDEX_HTML` is 46 bytes (SHA-256
/// `9a042df3871c5fb2ecf58ab0e13cd9ef5ada81304bc782ed49827fd5707258ce`).
const INDEX_HTML: &str = "1b2d0000a45e7860030e1cd22ca4878db1295fbcfd9e983dbc65eee51501";

/// `/assets/app.js`, 21 bytes (SHA-256
/// `f4b457f23429ca4f1afb44665306b56e6fa9deb620fc3eed01619fe328f65100`).
const APP_JS: &str = "0b0a80636f6e736f6c652e6c6f672822746175726922293b03";

/// `/icon.png`, 14 bytes (SHA-256
/// `84b15eaf2db8997aa5ba96d6ca7e29a6d30cb44e5a802d80033b122928b185c2`).
const ICON_PNG: &str = "8b068089504e470d0a1a0a00010203040503";

/// `/index.html` for the unsafe-key case, 27 bytes (SHA-256
/// `74262fd91e8cd47ae659a23a3856472937d0fd35dc6f4627e30d3f38b6040a8b`).
const SAFE_INDEX_HTML: &str = "1b1a0000a45f787ce0e426a208539377e02d4f0e";

/// `/../evil.txt` for the unsafe-key case, 17 bytes (SHA-256
/// `48d57bc3fcbd608b890753e317c6ede778a0fa0e4d8e933f601e8adf38a8f853`).
const EVIL_TRAVERSAL: &str = "0b088074726176657273616c207061796c6f616403";

/// Builds a 1:1 virtual-address container holding one interleaved
/// `[key_ptr, key_len, blob_ptr, blob_len, ...]` table in noise.
fn tauri_container(payloads: &[AssetVector]) -> Vec<u8> {
    let mut data = vec![0_u8; 512];
    let mut records: Vec<(u64, u64, u64, u64)> = Vec::new();
    for vector in payloads {
        let key_offset = data.len() as u64;
        data.extend_from_slice(vector.key.as_bytes());
        while data.len() % 8 != 0 {
            data.push(0);
        }
        let blob = compressed(vector.compressed);
        let blob_offset = data.len() as u64;
        data.extend_from_slice(&blob);
        records.push((key_offset, vector.key.len() as u64, blob_offset, blob.len() as u64));
    }
    while data.len() % 32 != 0 {
        data.push(0);
    }
    for (key_pointer, key_length, blob_pointer, blob_length) in records {
        data.extend_from_slice(&key_pointer.to_le_bytes());
        data.extend_from_slice(&key_length.to_le_bytes());
        data.extend_from_slice(&blob_pointer.to_le_bytes());
        data.extend_from_slice(&blob_length.to_le_bytes());
    }
    data.extend_from_slice(&[0x41_u8; 64]);
    data
}

fn tauri_payloads() -> Vec<AssetVector> {
    vec![
        AssetVector {
            key: "/index.html",
            plaintext: b"<!doctype html><html><body>hello</body></html>",
            compressed: INDEX_HTML,
        },
        AssetVector { key: "/assets/app.js", plaintext: b"console.log(\"tauri\");", compressed: APP_JS },
        AssetVector {
            key: "/icon.png",
            plaintext: &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05],
            compressed: ICON_PNG,
        },
    ]
}

/// Identity virtual-address map: the recorded address is the file offset.
fn identity_resolver(data: &[u8]) -> impl Fn(u64) -> Option<usize> + '_ {
    move |address| usize::try_from(address).ok().filter(|offset| *offset < data.len())
}

#[test]
fn tauri_assets_recovers_the_interleaved_table() {
    let payloads = tauri_payloads();
    let data = tauri_container(&payloads);
    let resolve = identity_resolver(&data);
    let report = scan_assets(&data, &resolve, &TauriAssetsOptions::default()).expect("scan");

    assert_eq!(report.total, payloads.len());
    assert!(!report.truncated);
    assert!(report.output_dir.is_none());
    let keys: Vec<&str> = report.assets.iter().map(|asset| asset.key.as_str()).collect();
    assert_eq!(keys, vec!["/index.html", "/assets/app.js", "/icon.png"]);
    for (asset, vector) in report.assets.iter().zip(&payloads) {
        assert_eq!(asset.decompressed_len, vector.plaintext.len());
    }
    assert!(report.assets[0].preview.contains("<html>"));
    assert!(report.assets[2].preview.is_empty(), "binary payloads keep an empty preview");
}

#[test]
fn tauri_assets_writes_files_and_cuts_at_the_limit() {
    let payloads = tauri_payloads();
    let data = tauri_container(&payloads);
    let resolve = identity_resolver(&data);
    let dir = temp_dir("tauri");
    let report = scan_assets(
        &data,
        &resolve,
        &TauriAssetsOptions { output_dir: Some(dir.display().to_string()), ..TauriAssetsOptions::default() },
    )
    .expect("scan with output");

    let written = report.assets[1].file.as_ref().expect("asset file").replace('\\', "/");
    assert!(written.ends_with("/assets/app.js"), "the key path is mirrored: {written}");
    assert_eq!(fs::read(&written).expect("read asset file"), payloads[1].plaintext);
    fs::remove_dir_all(&dir).expect("cleanup");

    let cut = scan_assets(&data, &resolve, &TauriAssetsOptions { limit: Some(1), ..TauriAssetsOptions::default() })
        .expect("cut scan");
    assert_eq!(cut.assets.len(), 1);
    assert_eq!(cut.total, 3);
    assert!(cut.truncated);

    let clamped = scan_assets(&data, &resolve, &TauriAssetsOptions { limit: Some(9_999), ..TauriAssetsOptions::default() })
        .expect("clamped scan");
    assert_eq!(clamped.assets.len(), 3);
    assert!(!clamped.truncated);
}

#[test]
fn tauri_assets_finds_no_table_in_flat_data() {
    let data: Vec<u8> = (0..4096_u32).map(|index| (index % 251) as u8).collect();
    let resolve = identity_resolver(&data);
    let report = scan_assets(&data, &resolve, &TauriAssetsOptions::default()).expect("noise scan");
    assert!(report.assets.is_empty());
    assert_eq!(report.total, 0);
    assert!(!report.truncated);
}

#[test]
fn tauri_assets_refuses_unsafe_asset_keys() {
    let payloads = vec![
        AssetVector { key: "/index.html", plaintext: b"<!doctype html><p>hello</p>", compressed: SAFE_INDEX_HTML },
        AssetVector { key: "/../evil.txt", plaintext: b"traversal payload", compressed: EVIL_TRAVERSAL },
    ];
    let data = tauri_container(&payloads);
    let resolve = identity_resolver(&data);
    let dir = temp_dir("tauri-bad");
    let error = scan_assets(
        &data,
        &resolve,
        &TauriAssetsOptions { output_dir: Some(dir.display().to_string()), ..TauriAssetsOptions::default() },
    )
    .expect_err("a traversal key must be refused");
    assert_eq!(error.code(), "binary/parse");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tauri_assets_require_a_pe_image() {
    let bytes = vec![0_u8; 512];
    let error = analyze(&bytes, &TauriAssetsOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
}
