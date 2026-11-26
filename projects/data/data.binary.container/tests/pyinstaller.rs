//! PyInstaller container facts: the `CArchive` cookie/TOC and the `PYZ`
//! module archive.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use data_binary_container::pyinstaller::{
    PyInstallerOptions, analyze_carchive, analyze_pyz, locate_cookie, looks_like_pyz,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("data-container-{}-{id}-{tag}", std::process::id()))
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "hex literal has an odd length");
    (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex digit")).collect()
}

/// Fixed Node.js zlib vectors used by the fixtures below.
const ZLIB_MODULE_ONE: &str = "789c7bccc0c0c0909659545ca2909b9f529a93aa909b58549c91980300689008ba";
const ZLIB_MODULE_TWO: &str = "789c7bccc0c0c0509c9a9c9f97a2909b9f529a93aa909b58549c9198030070d2090e";
const ZLIB_COMPRESSIBLE: &str = "789c4bcecf2d284a2d2ece4cca495548c6c949492c4904007d581113";
const ZLIB_PYZ: &str = "789c0b888c6238cdcbcbc5c0c0e05f31a7facc810307264c8b0c89593461f6fca05993574d981d113267e20c66868c091cbba00a02e6cc9a337f3a868a824b9c7cd551dc4989c5a9f1b9f929a539a91acc0c0c0c6e993c0c0c0c998a0c0c0c51ec05d9e97ab9f9296089904c5d9084120303830100f802302b";

/// Appends one big-endian word, as PyInstaller stores cookie and TOC words.
fn pyi_be32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends one `CArchive` TOC entry header plus its NUL-terminated name.
/// The fixed header is 18 bytes; the declared entry size counts the name
/// field, so the NUL padding is `19 - name length` bytes.
fn pyi_toc_entry(out: &mut Vec<u8>, offset: u32, length: u32, compress_length: u32, flags: u8, typecode: u8, name: &str) {
    pyi_be32(out, (18 + name.len() + 1) as u32);
    pyi_be32(out, offset);
    pyi_be32(out, length);
    pyi_be32(out, compress_length);
    out.push(flags);
    out.push(typecode);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
}

/// Writes one `marshal` short-ASCII string and one small int.
fn pyi_marshal_str(out: &mut Vec<u8>, text: &str) {
    out.push(b'Z');
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

fn pyi_marshal_int(out: &mut Vec<u8>, value: i32) {
    out.push(b'i');
    out.extend_from_slice(&value.to_le_bytes());
}

/// Writes the PYZ module dictionary: names mapped to triples.
fn pyi_pyz_toc(out: &mut Vec<u8>, modules: &[(&str, bool, u32, u32)]) {
    out.push(b'{');
    for (name, is_package, offset, length) in modules {
        pyi_marshal_str(out, name);
        out.push(b'(');
        out.extend_from_slice(&3i32.to_le_bytes());
        out.push(if *is_package { b'T' } else { b'F' });
        pyi_marshal_int(out, *offset as i32);
        pyi_marshal_int(out, *length as i32);
    }
    out.push(b'0');
}

/// The PYZ image: header, two fixed zlib-stored modules and the marshal
/// dictionary. The streams come from the fixed Node.js vectors, so the image
/// bytes are stable and `ZLIB_PYZ` is the matching compressed member.
fn pyz_image() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let module_one = b"\xe3\x00\x00\x00\x00first module marshal".to_vec();
    let module_two = b"\xe3\x00\x00\x00\x00second module marshal".to_vec();
    let stream_one = unhex(ZLIB_MODULE_ONE);
    let stream_two = unhex(ZLIB_MODULE_TWO);
    let mut pyz = b"PYZ\x00".to_vec();
    pyz.extend_from_slice(&[0xcb, 0x0d, 0x0d, 0x0a]);
    pyi_be32(&mut pyz, 12 + stream_one.len() as u32 + stream_two.len() as u32);
    pyz.extend_from_slice(&stream_one);
    pyz.extend_from_slice(&stream_two);
    let offset_one = 12;
    let offset_two = 12 + stream_one.len() as u32;
    pyi_pyz_toc(
        &mut pyz,
        &[("base_module", false, offset_one, stream_one.len() as u32), ("pkg.mod", true, offset_two, stream_two.len() as u32)],
    );
    (pyz, module_one, module_two)
}

/// A fake bootloader prefix with a complete `CArchive` appended: a raw script,
/// one zlib data entry and one zlib-compressed PYZ.
fn pyinstaller_bytes() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (pyz, module_one, module_two) = pyz_image();
    let mut exe = b"MZ\x90\x00 fake bootloader prefix".to_vec();
    let mut package: Vec<u8> = Vec::new();
    let script = b"\xe3\x00\x00\x00\x00main script marshal".to_vec();
    let script_offset = package.len() as u32;
    package.extend_from_slice(&script);
    let payload = b"compressible compressible compressible data".to_vec();
    let stream = unhex(ZLIB_COMPRESSIBLE);
    let data_offset = package.len() as u32;
    package.extend_from_slice(&stream);
    let pyz_stream = unhex(ZLIB_PYZ);
    let pyz_offset = package.len() as u32;
    package.extend_from_slice(&pyz_stream);
    let toc_offset = package.len() as u32;
    let mut toc = Vec::new();
    pyi_toc_entry(&mut toc, script_offset, script.len() as u32, script.len() as u32, 0, b's', "main.py");
    pyi_toc_entry(&mut toc, data_offset, payload.len() as u32, stream.len() as u32, 1, b'd', "payload.bin");
    pyi_toc_entry(&mut toc, pyz_offset, pyz.len() as u32, pyz_stream.len() as u32, 1, b'z', "PYZ-00.pyz");
    let toc_length = toc.len() as u32;
    package.extend_from_slice(&toc);
    exe.extend_from_slice(&package);
    exe.extend_from_slice(b"MEI\x0c\x0b\x0a\x0b\x0e");
    pyi_be32(&mut exe, (package.len() + 24) as u32);
    pyi_be32(&mut exe, toc_offset);
    pyi_be32(&mut exe, toc_length);
    pyi_be32(&mut exe, 312);
    (exe, module_one, module_two)
}

#[test]
fn carchive_indexes_entries_and_its_pyz() {
    let (bytes, _, _) = pyinstaller_bytes();
    assert!(locate_cookie(&bytes).is_some(), "the sniff finds the cookie");
    let report = analyze_carchive(&bytes, &PyInstallerOptions::default()).expect("decode the bundle");

    assert_eq!(report.archive_kind, "carchive");
    assert_eq!(report.version.as_deref(), Some("3.12"));
    assert_eq!(report.entry_count, 3);
    assert_eq!(report.entries.len(), 3);
    assert_eq!(report.entries[0].kind, "script");
    assert_eq!(report.entries[0].name, "main.py");
    assert!(!report.entries[0].compressed);
    assert_eq!(report.entries[1].kind, "data");
    assert!(report.entries[1].compressed);
    assert_eq!(report.entries[2].typecode, "z");
    assert_eq!(report.entries[2].module_count, Some(2));
    assert_eq!(report.entries[2].modules[0].name, "base_module");
    assert!(!report.entries[2].modules[0].is_package);
    assert_eq!(report.entries[2].modules[1].name, "pkg.mod");
    assert!(report.entries[2].modules[1].is_package);
    assert!(!report.truncated);

    let cut = analyze_carchive(&bytes, &PyInstallerOptions { entries_limit: 2, ..PyInstallerOptions::default() })
        .expect("cut listing");
    assert_eq!(cut.entries.len(), 2);
    assert!(cut.truncated);
}

#[test]
fn standalone_pyz_indexes_its_modules() {
    let (pyz, _, _) = pyz_image();
    assert!(looks_like_pyz(&pyz));
    let report = analyze_pyz(&pyz, &PyInstallerOptions::default()).expect("decode the PYZ");

    assert_eq!(report.archive_kind, "pyz");
    assert_eq!(report.python.as_deref(), Some("CPython 3.12"));
    assert_eq!(report.entry_count, 2);
    assert_eq!(report.modules[0].name, "base_module");
    assert_eq!(report.modules[1].name, "pkg.mod");
}

#[test]
fn carchive_extracts_entries_and_pyz_modules() {
    let (bytes, module_one, module_two) = pyinstaller_bytes();
    let dir = temp_dir("pyi");

    let report = analyze_carchive(
        &bytes,
        &PyInstallerOptions { extract_dir: Some(dir.display().to_string()), ..PyInstallerOptions::default() },
    )
    .expect("decode with extraction");

    let script = report.entries[0].extracted.as_ref().expect("script entry").replace('\\', "/");
    assert!(script.ends_with("/main.py"), "the entry name is mirrored: {script}");
    assert_eq!(fs::read(&script).expect("read script"), b"\xe3\x00\x00\x00\x00main script marshal");
    let payload = report.entries[1].extracted.as_ref().expect("data entry").replace('\\', "/");
    assert_eq!(fs::read(&payload).expect("read payload"), b"compressible compressible compressible data");
    assert!(report.entries[2].extracted.is_none(), "the PYZ blob itself is not written");
    let modules = &report.entries[2].modules;
    let first = modules[0].extracted.as_ref().expect("module one").replace('\\', "/");
    assert!(first.ends_with("/PYZ-00.pyz/base_module.pyc"), "modules land under the PYZ name: {first}");
    assert_eq!(fs::read(&first).expect("read module one"), module_one);
    let second = modules[1].extracted.as_ref().expect("module two").replace('\\', "/");
    assert_eq!(fs::read(&second).expect("read module two"), module_two);
    fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn carchive_rejects_a_broken_cookie() {
    let (mut bytes, _, _) = pyinstaller_bytes();
    // Claim a package longer than the file.
    let cookie = bytes.len() - 16;
    bytes[cookie..cookie + 4].copy_from_slice(&(u32::MAX).to_be_bytes());
    let error = analyze_carchive(&bytes, &PyInstallerOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
    assert!(error.message().contains("package length"), "the message names the field: {}", error.message());
    assert!(error.span().is_some(), "the rejected cookie span is reported");
}
