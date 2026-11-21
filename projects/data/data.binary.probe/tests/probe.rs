use boost_source::binary::MemorySource;
use data_binary_probe::{Confidence, ProbeSession, probe_memory};

#[test]
fn zip_magic_at_start() {
    let bytes = b"PK\x03\x04\x00\x00garbage".to_vec();
    let hits = probe_memory("sample.docx", bytes);
    assert!(hits.iter().any(|hit| hit.format_id == "zip"));
    assert!(hits.iter().all(|hit| hit.confidence == Confidence::Candidate));
}

#[test]
fn pdf_and_gzip_are_distinct_candidates() {
    let pdf = probe_memory("sample.pdf", b"%PDF-1.7\n".to_vec());
    assert!(pdf.iter().any(|hit| hit.format_id == "pdf"));

    let gzip = probe_memory("sample.gz", vec![0x1F, 0x8B, 0x08, 0x00]);
    assert!(gzip.iter().any(|hit| hit.format_id == "gzip"));

    let zlib = probe_memory("sample.zlib", vec![0x78, 0x9C, 0x00, 0x00]);
    assert!(zlib.iter().any(|hit| hit.format_id == "zlib"));
}

#[test]
fn builtins_cover_openx_container_signatures() {
    let cases: [(&[u8], &str); 8] = [
        (b"\x7fELF\x02\x01\x01", "elf"),
        (b"MZ\x90\x00", "pe"),
        (&[0xcf, 0xfa, 0xed, 0xfe], "macho"),
        (b"\0asm\x01\x00\x00\x00", "wasm"),
        (b"UnityFS\0\0\0\0\0\0\0", "unityfs"),
        (b"UnityWeb\0\0\0\0\0\0\0", "unityweb"),
        (b"CCON\x00\x01", "ccon"),
        (b"V1MMWX\x00\x00", "wxapkg"),
    ];
    for (bytes, expected) in cases {
        let hits = probe_memory("sample", bytes.to_vec());
        assert!(hits.iter().any(|hit| hit.format_id == expected), "missing {expected}");
    }
}

#[test]
fn tail_anchor_can_match_end_marker() {
    let mut session = ProbeSession::new();
    session.register(data_binary_probe::Signature {
        format_id: "zip-eocd".into(),
        anchor: data_binary_probe::ProbeAnchor::Tail { len: 22 },
        pattern: data_binary_probe::BytePattern::exact(b"PK\x05\x06"),
        priority: 1,
    });
    let mut bytes = vec![0u8; 30];
    bytes.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    let source = MemorySource::from_bytes("zip-tail", bytes);
    let hits = session.probe(&source);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].format_id, "zip-eocd");
}
