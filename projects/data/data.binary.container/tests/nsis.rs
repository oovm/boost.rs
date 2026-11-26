//! `NSIS` container facts: the first header, the blocks, the commands and the
//! `File` manifest.

use data_binary_container::nsis::{NsisOptions, analyze, locate};

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Decodes a hex literal into bytes.
fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "hex literal has an odd length");
    (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex digit")).collect()
}

/// `deflateRawSync` at level 6 of the 139-byte uncompressed header block
/// [`nsis_block`] builds: 52 stored bytes, Adler-32 `430378557`.
const RAW_NSIS_BLOCK: &str =
    "6360c0042e0c0c0c4c0c0c0c35507e371635c840044a33323030fcffffff3f36394e2c720c8905057aa915a90c19a93939f90c00";

/// Builds a synthetic uncompressed `NSIS` header block with two `File` commands.
fn nsis_block() -> Vec<u8> {
    let mut block = vec![0u8; 139];
    put_u32(&mut block, 0, 0);
    // block table: pages, sections, entries, strings, langtables, ctlcolors, font, data
    put_u32(&mut block, 20, 68); // entries.offset
    put_u32(&mut block, 24, 2); // entries.num
    put_u32(&mut block, 28, 124); // strings.offset
    put_u32(&mut block, 36, 139); // langtables.offset (ends the string table)
    // command 0: File, output = string char 1
    put_u32(&mut block, 68, 20);
    put_u32(&mut block, 68 + 4, 0);
    put_u32(&mut block, 68 + 8, 1);
    put_u32(&mut block, 68 + 12, u32::MAX);
    // command 1: File, output = string char 9
    put_u32(&mut block, 96, 20);
    put_u32(&mut block, 96 + 8, 9);
    put_u32(&mut block, 96 + 12, u32::MAX);
    // string table (latin-1): "", "app.exe", "hello"
    block[124] = 0x00;
    block[125..132].copy_from_slice(b"app.exe");
    block[132] = 0x00;
    block[133..138].copy_from_slice(b"hello");
    block[138] = 0x00;
    block
}

/// Assembles `prefix | first header | framing word | header block`.
fn container(header_size: u32, framing: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x600];
    let start = bytes.len();
    bytes.extend_from_slice(&[0u8; 32]);
    bytes[start + 4..start + 8].copy_from_slice(&[0xef, 0xbe, 0xad, 0xde]);
    bytes[start + 8..start + 20].copy_from_slice(b"NullsoftInst");
    put_u32(&mut bytes, start + 20, header_size);
    put_u32(&mut bytes, start + 28, framing);
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn nsis_decodes_an_uncompressed_container() {
    let block = nsis_block();
    let header_size = block.len() as u32;
    let bytes = container(header_size, header_size, &block);
    let start = 0x600;

    assert_eq!(locate(&bytes, None).expect("scan the first header"), start as u64);

    let report = analyze(&bytes, &NsisOptions { offset: Some(start as u64), ..NsisOptions::default() }).expect("nsis");
    assert_eq!(report.header.start_offset as usize, start);
    assert_eq!(report.header.method, "none");
    assert!(!report.header.solid);
    assert!(!report.unicode);
    assert_eq!(report.string_chars, 15);
    assert_eq!(report.strings, vec!["app.exe".to_string(), "hello".to_string()]);
    assert_eq!(report.command_count, 2);
    assert_eq!(report.commands[0].name, "File");
    assert_eq!(report.commands[0].rendered[1], "app.exe");
    assert_eq!(report.files.len(), 2);
    assert_eq!(report.files[0].output, "app.exe");
    assert_eq!(report.files[1].output, "hello");
    assert!(report.files.iter().all(|file| file.data_offset.is_none()));
    assert!(!report.truncated);
}

#[test]
fn nsis_decodes_a_deflate_container_from_a_fixed_vector() {
    let block = nsis_block();
    let header_size = block.len() as u32;
    let stream = unhex(RAW_NSIS_BLOCK);
    assert_eq!(stream.len(), 52, "the fixed raw DEFLATE vector has 52 stored bytes");

    // A non-solid framing word (bit 31 set) holds the compressed length in its
    // low 31 bits, then the raw DEFLATE stream follows.
    let bytes = container(header_size, 0x8000_0000 | stream.len() as u32, &stream);
    let report = analyze(&bytes, &NsisOptions { offset: Some(0x600), ..NsisOptions::default() }).expect("nsis");
    assert_eq!(report.header.start_offset, 0x600);
    assert_eq!(report.header.method, "deflate");
    assert!(!report.header.solid);
    assert_eq!(report.header.compressed_header_size, stream.len() as u32);
    // The decoded header block is exactly the plaintext [`nsis_block`] builds.
    assert_eq!(report.string_chars, 15);
    assert_eq!(report.strings, vec!["app.exe".to_string(), "hello".to_string()]);
    assert_eq!(report.command_count, 2);
    assert_eq!(report.commands[0].name, "File");
    assert_eq!(report.commands[0].rendered[1], "app.exe");
    assert_eq!(report.files.len(), 2);
}

#[test]
fn nsis_rejects_an_image_without_the_first_header() {
    let mut bytes = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    bytes.resize(0x600, 0);
    let error = analyze(&bytes, &NsisOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
}
