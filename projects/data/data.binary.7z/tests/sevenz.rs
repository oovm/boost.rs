//! Minimal pure-Rust 7z container builder plus indexing/decoding smoke tests.
//!
//! Every byte is constructed here with a packed-uint encoder mirroring the parser;
//! no fixture files or embedded binaries are used.

use data_binary_sevenz::{CodecLimits, SevenZDecodeError, SevenZIndexError, bcj_x86_decode, decode_entry_slice, index_7z_slice};

/// 7z magic bytes.
const SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
/// Coder id: stored copy.
const COPY: &[u8] = &[0x00];
/// Coder id: LZMA2.
const LZMA2: &[u8] = &[0x21];
/// Coder id: BCJ x86 branch filter.
const BCJ_X86: &[u8] = &[0x03, 0x03, 0x01, 0x03];
/// Coder id: Brotli (unsupported here, used for failure propagation).
const BROTLI: &[u8] = &[0x04];

/// Encodes a packed unsigned integer exactly as the 7z header format defines it.
fn packed(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..8usize {
        if value < (1u64 << (8 * i + (7 - i))) {
            out.push(((0xFFu32 << (8 - i)) as u8) | ((value >> (8 * i)) as u8));
            for j in 0..i {
                out.push((value >> (8 * j)) as u8);
            }
            return out;
        }
    }
    out.push(0xFF);
    for j in 0..8 {
        out.push((value >> (8 * j)) as u8);
    }
    out
}

/// Appends a most-significant-bit-first bitmap.
fn write_bits(out: &mut Vec<u8>, flags: &[bool]) {
    let start = out.len();
    out.resize(start + flags.len().div_ceil(8), 0);
    for (index, flag) in flags.iter().enumerate() {
        if *flag {
            out[start + index / 8] |= 0x80 >> (index % 8);
        }
    }
}

/// Encodes a name as UTF-16LE units.
fn utf16(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in name.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// One coder inside a folder.
struct Coder {
    id: Vec<u8>,
    properties: Vec<u8>,
}

fn coder(id: &[u8], properties: &[u8]) -> Coder {
    Coder { id: id.to_vec(), properties: properties.to_vec() }
}

/// One entry in the file table.
struct FileSpec {
    name: String,
    is_dir: bool,
}

fn file(name: &str) -> FileSpec {
    FileSpec { name: name.to_string(), is_dir: false }
}

fn dir(name: &str) -> FileSpec {
    FileSpec { name: name.to_string(), is_dir: true }
}

/// Builds the `Header` property tree for one folder and a file table.
///
/// `sub_sizes` splits the folder output across streamed files; the last sub-stream size
/// is derived by the parser from the folder total.
fn build_header(coders: &[Coder], pack_pos: u64, pack_len: u64, unpack_sizes: &[u64], sub_sizes: &[u64], files: &[FileSpec]) -> Vec<u8> {
    let mut out = vec![0x01]; // kHeader
    out.push(0x04); // kMainStreamsInfo
    out.push(0x06); // kPackInfo
    out.extend(packed(pack_pos)); // pack position, relative to the end of the signature header
    out.extend(packed(1)); // one pack stream
    out.push(0x09); // kSize
    out.extend(packed(pack_len));
    out.push(0x00); // kEnd
    out.push(0x07); // kUnpackInfo
    out.push(0x0B); // kFolder
    out.extend(packed(1)); // one folder
    out.push(0x00); // external == 0
    out.extend(packed(coders.len() as u64));
    for c in coders {
        let bits = (c.id.len() as u8) | if c.properties.is_empty() { 0 } else { 0x20 };
        out.push(bits);
        out.extend_from_slice(&c.id);
        if !c.properties.is_empty() {
            out.extend(packed(c.properties.len() as u64));
            out.extend_from_slice(&c.properties);
        }
    }
    // Linear chain: coder i output feeds coder i + 1 input.
    for i in 0..coders.len().saturating_sub(1) {
        out.extend(packed((i + 1) as u64)); // in index
        out.extend(packed(i as u64)); // out index
    }
    // One pack stream feeding the chain start: the free input index is implicit.
    out.push(0x0C); // kCodersUnpackSize
    for size in unpack_sizes {
        out.extend(packed(*size));
    }
    out.push(0x00); // kEnd (unpack info)
    if !sub_sizes.is_empty() {
        out.push(0x08); // kSubStreamsInfo
        out.push(0x0D); // kNumUnpackStream
        out.extend(packed(sub_sizes.len() as u64));
        out.push(0x09); // kSize
        for size in &sub_sizes[..sub_sizes.len() - 1] {
            out.extend(packed(*size));
        }
        out.push(0x00); // kEnd (sub-streams info)
    }
    out.push(0x00); // kEnd (streams info)

    out.push(0x05); // kFilesInfo
    out.extend(packed(files.len() as u64));
    let empty_flags: Vec<bool> = files.iter().map(|f| f.is_dir).collect();
    if empty_flags.iter().any(|flag| *flag) {
        let bitmap_len = empty_flags.len().div_ceil(8);
        out.push(0x0E); // kEmptyStream
        out.extend(packed(bitmap_len as u64));
        write_bits(&mut out, &empty_flags);
        let empty_file_flags: Vec<bool> = files.iter().filter(|f| f.is_dir).map(|_| false).collect();
        out.push(0x0F); // kEmptyFile
        out.extend(packed(empty_file_flags.len().div_ceil(8) as u64));
        write_bits(&mut out, &empty_file_flags);
    }
    let mut names = Vec::new();
    for (index, f) in files.iter().enumerate() {
        if index > 0 {
            names.extend_from_slice(&0u16.to_le_bytes());
        }
        names.extend(utf16(&f.name));
    }
    out.push(0x11); // kName
    out.extend(packed(1 + names.len() as u64));
    out.push(0x00); // external == 0
    out.extend(names);
    out.push(0x00); // kEnd (files info)
    out
}

/// Assembles a complete container: signature header, pack data, then the next header.
fn build_archive(coders: &[Coder], pack_data: &[u8], unpack_sizes: &[u64], sub_sizes: &[u64], files: &[FileSpec]) -> Vec<u8> {
    // Layout: signature header, then the pack data, then the next header. The header's
    // pack position is zero because the pack stream starts right at the signature end.
    let header = build_header(coders, 0, pack_data.len() as u64, unpack_sizes, sub_sizes, files);
    let mut out = SIGNATURE.to_vec();
    out.extend_from_slice(&[0, 4]); // version 0.4
    let mut record = Vec::new();
    record.extend((pack_data.len() as u64).to_le_bytes()); // next header offset
    record.extend((header.len() as u64).to_le_bytes());
    record.extend(0u32.to_le_bytes()); // next header crc omitted
    out.extend(data_checksum::crc32(&record).to_le_bytes());
    out.extend(record);
    out.extend(pack_data);
    out.extend(header);
    out
}

/// Builds an LZMA2 stream made of uncompressed chunks plus the end marker.
fn lzma2_uncompressed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut first = true;
    for chunk in bytes.chunks(65_536) {
        out.push(if first { 0x01 } else { 0x02 });
        first = false;
        out.extend_from_slice(&((chunk.len() - 1) as u16).to_be_bytes());
        out.extend_from_slice(chunk);
    }
    out.push(0x00);
    out
}

#[test]
fn indexes_copy_archive() {
    let payload = b"boost sevenz copy payload";
    let archive = build_archive(&[coder(COPY, &[])], payload, &[payload.len() as u64], &[payload.len() as u64], &[file("a.txt"), dir("d")]);
    let index = index_7z_slice(&archive).expect("copy archive indexes");
    assert_eq!(index.version, (0, 4));
    assert_eq!(index.methods, vec![COPY.to_vec()]);
    assert_eq!(index.folder_count(), 1);
    assert_eq!(index.entry_count(), 2);
    assert_eq!(index.total_unpack_size, payload.len() as u64);
    assert_eq!(index.entries[0].name, "a.txt");
    assert_eq!(index.entries[0].size, Some(payload.len() as u64));
    assert_eq!(index.entries[0].offset, Some(0));
    assert!(!index.entries[0].is_dir);
    assert!(!index.entries[0].is_empty_stream);
    assert!(index.entries[1].is_dir);
    assert!(index.entries[1].is_empty_stream);
    assert_eq!(index.entries[1].folder_index, None);
}

#[test]
fn decodes_copy_entry() {
    let payload = b"boost sevenz copy payload";
    let archive = build_archive(&[coder(COPY, &[])], payload, &[payload.len() as u64], &[payload.len() as u64], &[file("a.txt")]);
    let index = index_7z_slice(&archive).expect("indexes");
    let decoded = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect("decodes");
    assert_eq!(decoded, payload);
}

#[test]
fn decodes_lzma2_entry() {
    let payload = vec![0x41u8; 300];
    let pack = lzma2_uncompressed(&payload);
    let archive = build_archive(&[coder(LZMA2, &[0x14])], &pack, &[payload.len() as u64], &[payload.len() as u64], &[file("solid.bin")]);
    let index = index_7z_slice(&archive).expect("indexes");
    assert_eq!(index.methods, vec![LZMA2.to_vec()]);
    let decoded = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect("decodes");
    assert_eq!(decoded, payload);
}

#[test]
fn decodes_solid_lzma2_substreams() {
    let payload = b"abcdefgh";
    let pack = lzma2_uncompressed(payload);
    let archive = build_archive(&[coder(LZMA2, &[0x14])], &pack, &[payload.len() as u64], &[3, 5], &[file("one.bin"), file("two.bin")]);
    let index = index_7z_slice(&archive).expect("indexes");
    assert_eq!(index.entries[0].offset, Some(0));
    assert_eq!(index.entries[1].offset, Some(3));
    let first = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect("first");
    let second = decode_entry_slice(&archive, &index, 1, CodecLimits::new()).expect("second");
    assert_eq!(first, b"abc");
    assert_eq!(second, b"defgh");
}

#[test]
fn decodes_bcj_filter_chain() {
    // Absolute call target 0x00000004 at instruction offset 1 decodes to the
    // displacement 4 - (1 + 5) = -2; the second call is left untouched because its
    // stored address 0x12345678 does not sit in the centered 25-bit convertible range.
    let encoded: Vec<u8> = vec![0x00, 0xE8, 0x04, 0x00, 0x00, 0x00, 0x00, 0xE9, 0x78, 0x56, 0x34, 0x12];
    let expected: Vec<u8> = vec![0x00, 0xE8, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0xE9, 0x78, 0x56, 0x34, 0x12];
    let mut direct = encoded.clone();
    bcj_x86_decode(&mut direct);
    assert_eq!(direct, expected);

    let pack = lzma2_uncompressed(&encoded);
    let archive = build_archive(
        &[coder(LZMA2, &[0x14]), coder(BCJ_X86, &[])],
        &pack,
        &[encoded.len() as u64, encoded.len() as u64],
        &[encoded.len() as u64],
        &[file("code.bin")],
    );
    let index = index_7z_slice(&archive).expect("indexes");
    assert_eq!(index.methods, vec![LZMA2.to_vec(), BCJ_X86.to_vec()]);
    let decoded = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect("decodes");
    assert_eq!(decoded, expected);
}

#[test]
fn decodes_encoded_header() {
    // Spec layout: every pack position is relative to the end of the start header, so
    // the data pack sits at offset 32, then the encoded header's own pack stream, and
    // the encoded header block last.
    let payload = b"encoded header payload";
    let data_pack = lzma2_uncompressed(payload);
    // The inner header references the data pack at the start of the data area, so its
    // pack position is zero.
    let inner =
        build_header(&[coder(LZMA2, &[0x14])], 0, data_pack.len() as u64, &[payload.len() as u64], &[payload.len() as u64], &[file("x.bin")]);
    let header_pack = lzma2_uncompressed(&inner);

    let mut encoded = vec![0x17]; // kEncodedHeader
    encoded.push(0x06); // kPackInfo
    encoded.extend(packed(data_pack.len() as u64)); // own pack stream follows the data pack
    encoded.extend(packed(1));
    encoded.push(0x09); // kSize
    encoded.extend(packed(header_pack.len() as u64));
    encoded.push(0x00); // kEnd (pack info)
    encoded.push(0x07); // kUnpackInfo
    encoded.push(0x0B); // kFolder
    encoded.extend(packed(1));
    encoded.push(0x00); // external
    encoded.extend(packed(1)); // one coder
    encoded.push(0x01 | 0x20); // LZMA2 id with properties
    encoded.extend(LZMA2);
    encoded.extend(packed(1));
    encoded.push(0x14); // dictionary property byte
    encoded.push(0x0C); // kCodersUnpackSize
    encoded.extend(packed(inner.len() as u64));
    encoded.push(0x00); // kEnd (unpack info)
    encoded.push(0x00); // kEnd (streams info)

    let mut out = SIGNATURE.to_vec();
    out.extend_from_slice(&[0, 4]);
    let mut record = Vec::new();
    record.extend((data_pack.len() as u64 + header_pack.len() as u64).to_le_bytes()); // encoded block follows both packs
    record.extend((encoded.len() as u64).to_le_bytes());
    record.extend(0u32.to_le_bytes());
    out.extend(data_checksum::crc32(&record).to_le_bytes());
    out.extend(record);
    out.extend(&data_pack);
    out.extend(&header_pack);
    out.extend(&encoded);

    let archive = out;
    let index = index_7z_slice(&archive).expect("encoded header indexes");
    assert_eq!(index.entry_count(), 1);
    assert_eq!(index.entries[0].name, "x.bin");
    let decoded = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect("decodes");
    assert_eq!(decoded, payload);
}

#[test]
fn indexes_empty_container() {
    let mut out = SIGNATURE.to_vec();
    out.extend_from_slice(&[0, 4]);
    out.extend(0u32.to_le_bytes()); // zero crc
    out.extend([0u8; 20]); // zeroed start header record
    let index = index_7z_slice(&out).expect("empty container");
    assert_eq!(index.entry_count(), 0);
    assert_eq!(index.folder_count(), 0);
    assert_eq!(index.total_unpack_size, 0);
}

#[test]
fn indexing_is_deterministic() {
    let payload = b"deterministic payload bytes";
    let pack = lzma2_uncompressed(payload);
    let archive = build_archive(&[coder(LZMA2, &[0x14])], &pack, &[payload.len() as u64], &[payload.len() as u64], &[file("d.bin")]);
    let first = index_7z_slice(&archive).expect("first index");
    let second = index_7z_slice(&archive).expect("second index");
    assert_eq!(first, second);
}

#[test]
fn rejects_bad_signature() {
    let mut archive = build_archive(&[coder(COPY, &[])], b"x", &[1], &[1], &[file("a")]);
    archive[0] ^= 0xFF;
    assert_eq!(index_7z_slice(&archive), Err(SevenZIndexError::NotSevenZ));
}

#[test]
fn rejects_start_header_crc() {
    let mut archive = build_archive(&[coder(COPY, &[])], b"x", &[1], &[1], &[file("a")]);
    archive[20] ^= 0x01; // corrupt the start header record
    assert!(matches!(index_7z_slice(&archive), Err(SevenZIndexError::StartHeaderCrcMismatch { .. })));
}

#[test]
fn rejects_truncated_header() {
    let payload = b"truncation probe payload";
    let archive = build_archive(&[coder(COPY, &[])], payload, &[payload.len() as u64], &[payload.len() as u64], &[file("t.bin")]);
    let truncated = &archive[..archive.len() - 4];
    assert!(matches!(index_7z_slice(truncated), Err(SevenZIndexError::Truncated { .. } | SevenZIndexError::HeaderOutOfBounds)));
}

#[test]
fn propagates_unsupported_coder() {
    let archive = build_archive(&[coder(BROTLI, &[])], b"data", &[4], &[4], &[file("b.bin")]);
    let index = index_7z_slice(&archive).expect("container indexes");
    let error = decode_entry_slice(&archive, &index, 0, CodecLimits::new()).expect_err("brotli must fail");
    assert!(matches!(error, SevenZDecodeError::UnsupportedCoder { .. }));
}

#[test]
fn enforces_output_limit() {
    let payload = vec![0x42u8; 4096];
    let pack = lzma2_uncompressed(&payload);
    let archive = build_archive(&[coder(LZMA2, &[0x14])], &pack, &[payload.len() as u64], &[payload.len() as u64], &[file("big.bin")]);
    let index = index_7z_slice(&archive).expect("indexes");
    let error = decode_entry_slice(&archive, &index, 0, CodecLimits::new().with_max_output_bytes(1024)).expect_err("limit must bind");
    assert!(matches!(error, SevenZDecodeError::Codec(data_compression::CodecError::OutputLimitExceeded { .. })));
}
