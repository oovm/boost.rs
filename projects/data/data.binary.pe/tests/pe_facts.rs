//! `PE` 读取侧事实的集成测试。
//!
//! 每个夹具都是在内存中构造的小字节数组，只覆盖被测事实所需的布局；
//! 断言与产品侧同名测试保持一致，顺序与取值不变。

use data_binary_pe::pe::{
    parse_pe, read_bound_imports, read_certificates, read_debug_directory, read_delay_imports, read_exception_directory, read_load_config,
    read_mitigations, read_relocations, read_resources, read_rich_header, read_tls_directory, read_version_resources,
};

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[allow(clippy::too_many_arguments)]
fn put_section(
    bytes: &mut [u8],
    offset: usize,
    name: &[u8],
    virtual_size: u32,
    virtual_address: u32,
    raw_size: u32,
    raw_pointer: u32,
    characteristics: u32,
) {
    let name_length = name.len().min(8);
    bytes[offset..offset + name_length].copy_from_slice(&name[..name_length]);
    put_u32(bytes, offset + 8, virtual_size);
    put_u32(bytes, offset + 12, virtual_address);
    put_u32(bytes, offset + 16, raw_size);
    put_u32(bytes, offset + 20, raw_pointer);
    put_u32(bytes, offset + 36, characteristics);
}

/// 两节 PE32+：PE 头在 0x80，节数据在 0x200/0x400。
fn pe_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x600];
    bytes[0] = 0x4d;
    bytes[1] = 0x5a;
    put_u32(&mut bytes, 0x3c, 0x80);
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    put_u16(&mut bytes, 0x84, 0x8664);
    put_u16(&mut bytes, 0x86, 2);
    put_u16(&mut bytes, 0x94, 0xf0);
    put_u16(&mut bytes, 0x98, 0x20b);
    put_u32(&mut bytes, 0x98 + 16, 0x1000);
    put_u64(&mut bytes, 0x98 + 24, 0x1_4000_0000);
    put_u32(&mut bytes, 0x98 + 60, 0x200);
    put_section(&mut bytes, 0x188, b".text", 0x100, 0x1000, 0x200, 0x200, 0x6000_0020);
    put_section(&mut bytes, 0x1b0, b".data", 0x100, 0x2000, 0x200, 0x400, 0xc000_0040);
    bytes
}

/// `pe_bytes()` 加一条位于 RVA 0x2000 的 `CodeView` 调试项（`RSDS`）。
fn pe_debug_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 6 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 6 * 8 + 4, 28);

    let entry = 0x400;
    put_u32(&mut bytes, entry + 4, 0x1122_3344);
    put_u16(&mut bytes, entry + 8, 1);
    put_u16(&mut bytes, entry + 10, 2);
    put_u32(&mut bytes, entry + 12, 2);
    put_u32(&mut bytes, entry + 16, 44);
    put_u32(&mut bytes, entry + 20, 0x2040);
    put_u32(&mut bytes, entry + 24, 0x440);

    let record = 0x440;
    bytes[record..record + 4].copy_from_slice(b"RSDS");
    bytes[record + 4..record + 8].copy_from_slice(&0x1122_3344u32.to_le_bytes());
    bytes[record + 8..record + 10].copy_from_slice(&0x5566u16.to_le_bytes());
    bytes[record + 10..record + 12].copy_from_slice(&0x7788u16.to_le_bytes());
    bytes[record + 12..record + 20].copy_from_slice(&[0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00]);
    put_u32(&mut bytes, record + 20, 7);
    bytes[record + 24..record + 44].copy_from_slice(b"C:\\build\\sample.pdb\0");
    bytes
}

/// `pe_bytes()` 加一条 `NB10` 调试项，保留其偏移与时间戳。
fn pe_debug_nb10_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 6 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 6 * 8 + 4, 28);

    let path = b"C:\\legacy.pdb\0";
    let size = (16 + path.len()) as u32;
    let entry = 0x400;
    put_u32(&mut bytes, entry + 4, 0x1122_3344);
    put_u16(&mut bytes, entry + 8, 1);
    put_u16(&mut bytes, entry + 10, 2);
    put_u32(&mut bytes, entry + 12, 2);
    put_u32(&mut bytes, entry + 16, size);
    put_u32(&mut bytes, entry + 20, 0x2040);
    put_u32(&mut bytes, entry + 24, 0x440);

    let record = 0x440;
    bytes[record..record + 4].copy_from_slice(b"NB10");
    put_u32(&mut bytes, record + 4, 0xaabb_ccdd);
    put_u32(&mut bytes, record + 8, 0x1122_3344);
    put_u32(&mut bytes, record + 12, 7);
    bytes[record + 16..record + 16 + path.len()].copy_from_slice(path);
    bytes
}

#[test]
fn pe_debug_directory_reports_codeview_records() {
    let bytes = pe_debug_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let entries = read_debug_directory(&image, &bytes).expect("debug");

    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.stable_type_name(), "codeview");
    assert_eq!(entry.timestamp, 0x1122_3344);
    assert_eq!(entry.major_version, 1);
    assert_eq!(entry.minor_version, 2);
    assert_eq!(entry.size_of_data, 44);
    assert_eq!(entry.address_of_raw_data, 0x2040);
    assert_eq!(entry.pointer_to_raw_data, 0x440);

    let code_view = entry.code_view.as_ref().expect("RSDS record");
    assert_eq!(code_view.signature, "RSDS");
    assert_eq!(code_view.guid_string(), Some("11223344-5566-7788-99aa-bbccddeeff00".to_string()));
    assert_eq!(code_view.age, 7);
    assert_eq!(code_view.offset, None);
    assert_eq!(code_view.timestamp, None);
    assert_eq!(code_view.path, "C:\\build\\sample.pdb");
}

#[test]
fn pe_debug_directory_reads_nb10_offset_and_timestamp() {
    let bytes = pe_debug_nb10_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let entries = read_debug_directory(&image, &bytes).expect("debug");

    assert_eq!(entries.len(), 1);
    let code_view = entries[0].code_view.as_ref().expect("NB10 record");
    assert_eq!(code_view.signature, "NB10");
    assert_eq!(code_view.guid_string(), None);
    assert_eq!(code_view.age, 7);
    assert_eq!(code_view.offset, Some(0xaabb_ccdd));
    assert_eq!(code_view.timestamp, Some(0x1122_3344));
    assert_eq!(code_view.path, "C:\\legacy.pdb");
}

#[test]
fn pe_without_a_debug_directory_reports_no_entries() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    assert!(read_debug_directory(&image, &bytes).expect("debug").is_empty());
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(|unit| unit.to_le_bytes()).collect()
}

fn version_block(key: &str, value_length: u16, value_type: u16, value: &[u8], children: &[u8]) -> Vec<u8> {
    let mut block = vec![0u8, 0];
    block.extend_from_slice(&value_length.to_le_bytes());
    block.extend_from_slice(&value_type.to_le_bytes());
    block.extend_from_slice(&utf16z(key));
    while block.len() % 4 != 0 {
        block.push(0);
    }
    block.extend_from_slice(value);
    while block.len() % 4 != 0 {
        block.push(0);
    }
    block.extend_from_slice(children);
    while block.len() % 4 != 0 {
        block.push(0);
    }
    let length = block.len() as u16;
    block[0..2].copy_from_slice(&length.to_le_bytes());
    block
}

/// 一份 `VS_VERSIONINFO` 载荷：固定信息 1.2.3.4 / 5.6.7.8 与两个字符串。
fn version_payload() -> Vec<u8> {
    let mut fixed = vec![0u8; 52];
    put_u32(&mut fixed, 0, 0xFEEF_04BD);
    put_u32(&mut fixed, 4, 0x0001_0000);
    put_u32(&mut fixed, 8, 0x0001_0002);
    put_u32(&mut fixed, 12, 0x0003_0004);
    put_u32(&mut fixed, 16, 0x0005_0006);
    put_u32(&mut fixed, 20, 0x0007_0008);
    put_u32(&mut fixed, 24, 0x3f);
    put_u32(&mut fixed, 32, 0x4);
    put_u32(&mut fixed, 36, 0x1);

    let company = version_block("CompanyName", 8, 1, &utf16z("OpenCTF"), &[]);
    let description = version_block("FileDescription", 7, 1, &utf16z("Sample"), &[]);
    let mut table_children = company;
    table_children.extend_from_slice(&description);
    let table = version_block("040904B0", 0, 1, &[], &table_children);
    let string_file_info = version_block("StringFileInfo", 0, 1, &[], &table);
    version_block("VS_VERSION_INFO", 52, 0, &fixed, &string_file_info)
}

/// `.rsrc` 节承载一份 `RT_VERSION` 资源的 PE32+ 镜像。
fn pe_version_bytes() -> Vec<u8> {
    let payload = version_payload();
    let mut bytes = vec![0u8; 0x600];
    bytes[0..2].copy_from_slice(b"MZ");
    put_u32(&mut bytes, 0x3c, 0x80);
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    put_u16(&mut bytes, 0x84, 0x8664);
    put_u16(&mut bytes, 0x86, 1);
    put_u16(&mut bytes, 0x94, 0xf0);
    put_u16(&mut bytes, 0x98, 0x20b);
    put_u32(&mut bytes, 0x98 + 16, 0x1000);
    put_u64(&mut bytes, 0x98 + 24, 0x1_4000_0000);
    put_u32(&mut bytes, 0x98 + 56, 0x2000);
    put_u32(&mut bytes, 0x98 + 60, 0x200);
    put_u32(&mut bytes, 0x98 + 112 + 2 * 8, 0x1000);
    put_u32(&mut bytes, 0x98 + 112 + 2 * 8 + 4, 0x400);
    put_section(&mut bytes, 0x188, b".rsrc", 0x400, 0x1000, 0x400, 0x200, 0x4000_0040);

    // 根 -> 类型 (16) -> 名称 (1) -> 语言 (0x0409) -> 数据项。
    put_u16(&mut bytes, 0x200 + 12, 0);
    put_u16(&mut bytes, 0x200 + 14, 1);
    put_u32(&mut bytes, 0x210, 16);
    put_u32(&mut bytes, 0x210 + 4, 0x8000_0020);
    put_u16(&mut bytes, 0x220 + 12, 0);
    put_u16(&mut bytes, 0x220 + 14, 1);
    put_u32(&mut bytes, 0x230, 1);
    put_u32(&mut bytes, 0x230 + 4, 0x8000_0060);
    put_u16(&mut bytes, 0x260 + 12, 0);
    put_u16(&mut bytes, 0x260 + 14, 1);
    put_u32(&mut bytes, 0x270, 0x0409);
    put_u32(&mut bytes, 0x270 + 4, 0x80);
    put_u32(&mut bytes, 0x280, 0x1090);
    put_u32(&mut bytes, 0x280 + 4, payload.len() as u32);
    put_u32(&mut bytes, 0x280 + 8, 0x04b0);
    bytes[0x290..0x290 + payload.len()].copy_from_slice(&payload);
    bytes
}

#[test]
fn pe_version_resource_reports_fixed_info_and_strings() {
    let bytes = pe_version_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_version_resources(&image, &bytes).expect("version report");

    assert!(report.present);
    let fixed = report.fixed.expect("fixed file info");
    assert_eq!(fixed.file_version, "1.2.3.4");
    assert_eq!(fixed.product_version, "5.6.7.8");
    assert_eq!(fixed.file_flags_mask, 0x3f);
    assert_eq!(fixed.file_os, 0x4);
    assert_eq!(fixed.file_type, 0x1);

    assert_eq!(report.total, 2);
    assert!(!report.truncated);
    let company = report.strings.iter().find(|entry| entry.key == "CompanyName").expect("CompanyName");
    assert_eq!(company.value, "OpenCTF");
    assert_eq!(company.language_id, 0x0409);
    assert_eq!(company.codepage, 0x04b0);
    let description = report.strings.iter().find(|entry| entry.key == "FileDescription").expect("FileDescription");
    assert_eq!(description.value, "Sample");
}

#[test]
fn pe_without_version_resource_reports_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_version_resources(&image, &bytes).expect("version report");
    assert!(!report.present);
    assert!(report.fixed.is_none());
    assert!(report.strings.is_empty());
    assert!(report.note.is_none());
}

#[test]
fn pe_resources_walk_the_directory_tree() {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 2 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 2 * 8 + 4, 0x100);

    let base = 0x400;
    put_u16(&mut bytes, base + 12, 1);
    put_u16(&mut bytes, base + 14, 1);
    put_u32(&mut bytes, base + 0x10, 0x8000_00c0);
    put_u32(&mut bytes, base + 0x14, 0x8000_0020);
    put_u32(&mut bytes, base + 0x18, 16);
    put_u32(&mut bytes, base + 0x1c, 0x8000_0070);

    put_u16(&mut bytes, base + 0x2c, 1);
    put_u32(&mut bytes, base + 0x30, 1);
    put_u32(&mut bytes, base + 0x34, 0x8000_0040);
    put_u16(&mut bytes, base + 0x4c, 1);
    put_u32(&mut bytes, base + 0x50, 0x409);
    put_u32(&mut bytes, base + 0x54, 0x60);
    put_u32(&mut bytes, base + 0x60, 0x1000);
    put_u32(&mut bytes, base + 0x64, 0x10);
    put_u32(&mut bytes, base + 0x68, 0);

    put_u16(&mut bytes, base + 0x7c, 1);
    put_u32(&mut bytes, base + 0x80, 1);
    put_u32(&mut bytes, base + 0x84, 0x8000_0090);
    put_u16(&mut bytes, base + 0x9c, 1);
    put_u32(&mut bytes, base + 0xa0, 0x409);
    put_u32(&mut bytes, base + 0xa4, 0xb0);
    put_u32(&mut bytes, base + 0xb0, 0x1000);
    put_u32(&mut bytes, base + 0xb4, 0x20);
    put_u32(&mut bytes, base + 0xb8, 1200);

    put_u16(&mut bytes, base + 0xc0, 4);
    for (index, unit) in "TEST".encode_utf16().enumerate() {
        put_u16(&mut bytes, base + 0xc2 + index * 2, unit);
    }

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_resources(&image, &bytes).expect("PE resources");

    assert!(report.present);
    assert_eq!(report.total, 2);
    assert!(!report.truncated);
    assert_eq!(report.resources.len(), 2);

    assert_eq!(report.resources[0].resource_type, "TEST");
    assert_eq!(report.resources[0].name, "#1");
    assert_eq!(report.resources[0].language, 0x409);
    assert_eq!(report.resources[0].data_rva, 0x1000);
    assert_eq!(report.resources[0].size, 0x10);
    assert_eq!(report.resources[0].code_page, 0);

    assert_eq!(report.resources[1].resource_type, "VERSION");
    assert_eq!(report.resources[1].language, 0x409);
    assert_eq!(report.resources[1].size, 0x20);
    assert_eq!(report.resources[1].code_page, 1200);
}

#[test]
fn pe_resources_report_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_resources(&image, &bytes).expect("PE resources");
    assert!(!report.present);
    assert_eq!(report.total, 0);
    assert!(report.resources.is_empty());
}

/// `pe_bytes()` 加两个位于数据目录 5 之后的基址重定位块。
fn pe_relocations_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 5 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 5 * 8 + 4, 24);

    let first = 0x400;
    put_u32(&mut bytes, first, 0x1000);
    put_u32(&mut bytes, first + 4, 16);
    put_u16(&mut bytes, first + 8, 0x3010);
    put_u16(&mut bytes, first + 10, 0x0000);
    put_u16(&mut bytes, first + 12, 0xa000);
    put_u16(&mut bytes, first + 14, 0x3018);

    let second = 0x410;
    put_u32(&mut bytes, second, 0x2000);
    put_u32(&mut bytes, second + 4, 8);
    bytes
}

#[test]
fn pe_relocations_decode_blocks_and_fixup_types() {
    let bytes = pe_relocations_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_relocations(&image, &bytes).expect("PE relocations");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.total_blocks, 2);
    assert_eq!(report.total_entries, 4);
    assert_eq!(report.blocks.len(), 2);

    assert_eq!(report.blocks[0].page_rva, 0x1000);
    assert_eq!(report.blocks[0].block_size, 16);
    assert_eq!(report.blocks[0].entries, 4);
    assert_eq!(report.blocks[0].kinds.len(), 3);
    assert_eq!(report.blocks[0].kinds[0].kind, "ABSOLUTE");
    assert_eq!(report.blocks[0].kinds[0].count, 1);
    assert_eq!(report.blocks[0].kinds[1].kind, "HIGHLOW");
    assert_eq!(report.blocks[0].kinds[1].count, 2);
    assert_eq!(report.blocks[0].kinds[2].kind, "DIR64");

    assert_eq!(report.blocks[1].page_rva, 0x2000);
    assert_eq!(report.blocks[1].entries, 0);
    assert!(report.blocks[1].kinds.is_empty());

    assert_eq!(report.kinds.len(), 3);
    assert_eq!(report.kinds[0].kind, "ABSOLUTE");
    assert_eq!(report.kinds[1].count, 2);
    assert_eq!(report.kinds[2].kind, "DIR64");
}

#[test]
fn pe_relocations_report_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_relocations(&image, &bytes).expect("PE relocations");
    assert!(!report.present);
    assert_eq!(report.total_blocks, 0);
    assert_eq!(report.total_entries, 0);
    assert!(report.blocks.is_empty());
    assert!(report.kinds.is_empty());
}

/// `pe_bytes()` 加两个位于数据目录 3 之后的 x64 运行时函数。
fn pe_exception_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 3 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 3 * 8 + 4, 24);

    put_u32(&mut bytes, 0x400, 0x1010);
    put_u32(&mut bytes, 0x404, 0x1040);
    put_u32(&mut bytes, 0x408, 0x2060);
    put_u32(&mut bytes, 0x40c, 0x1050);
    put_u32(&mut bytes, 0x410, 0x1080);
    put_u32(&mut bytes, 0x414, 0x2070);

    let handler = 0x460;
    bytes[handler] = 0x01 | (0x01 << 3);
    bytes[handler + 1] = 0x10;
    bytes[handler + 2] = 5;
    bytes[handler + 3] = (2 << 4) | 3;

    let chained = 0x470;
    bytes[chained] = 0x02 | (0x04 << 3);
    bytes
}

#[test]
fn pe_exception_decodes_x64_runtime_functions() {
    let bytes = pe_exception_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_exception_directory(&image, &bytes).expect("PE exception directory");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.machine, 0x8664);
    assert_eq!(report.machine_name, "amd64");
    assert_eq!(report.entry_format, "x64");
    assert_eq!(report.entry_size, Some(12));
    assert_eq!(report.total, 2);
    assert_eq!(report.functions.len(), 2);

    let first = &report.functions[0];
    assert_eq!(first.begin_address, 0x1010);
    assert_eq!(first.end_address, 0x1040);
    assert_eq!(first.unwind_info, 0x2060);
    assert_eq!(first.unwind_offset, Some(0x460));
    let unwind = first.unwind.as_ref().expect("unwind header");
    assert_eq!(unwind.version, 1);
    assert_eq!(unwind.flags, 1);
    assert_eq!(unwind.flag_names, vec!["EHANDLER"]);
    assert_eq!(unwind.prolog_size, 0x10);
    assert_eq!(unwind.code_count, 5);
    assert_eq!(unwind.frame_register, 3);
    assert_eq!(unwind.frame_offset, 2);
    assert!(!unwind.chained);

    let second = &report.functions[1];
    assert_eq!(second.begin_address, 0x1050);
    assert_eq!(second.unwind_info, 0x2070);
    let unwind = second.unwind.as_ref().expect("chained unwind header");
    assert_eq!(unwind.version, 2);
    assert_eq!(unwind.flag_names, vec!["CHAININFO"]);
    assert!(unwind.chained);
}

#[test]
fn pe_exception_masks_arm64_packed_entries() {
    let mut bytes = pe_exception_bytes();
    put_u16(&mut bytes, 0x84, 0xaa64);
    put_u32(&mut bytes, 0x98 + 112 + 3 * 8 + 4, 8);
    put_u32(&mut bytes, 0x400, 0x1012);
    put_u32(&mut bytes, 0x404, 0x2060);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_exception_directory(&image, &bytes).expect("ARM64 exception directory");
    assert!(report.present);
    assert_eq!(report.machine_name, "arm64");
    assert_eq!(report.entry_format, "arm64");
    assert_eq!(report.entry_size, Some(8));
    assert_eq!(report.total, 1);
    assert_eq!(report.functions.len(), 1);
    assert_eq!(report.functions[0].begin_address, 0x1010);
    assert_eq!(report.functions[0].end_address, 0);
    assert_eq!(report.functions[0].unwind_info, 0x2060);
    assert!(report.functions[0].unwind.is_none());
}

#[test]
fn pe_exception_reports_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_exception_directory(&image, &bytes).expect("PE exception directory");
    assert!(!report.present);
    assert_eq!(report.total, 0);
    assert!(report.functions.is_empty());
}

/// `pe_bytes()` 加两个位于数据目录 13 之后的延迟加载描述符。
fn pe_delay_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 13 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 13 * 8 + 4, 96);

    put_u32(&mut bytes, 0x400 + 4, 0x20a0);
    put_u32(&mut bytes, 0x400 + 8, 0x2008);
    put_u32(&mut bytes, 0x400 + 12, 0x2070);
    put_u32(&mut bytes, 0x400 + 16, 0x2060);
    put_u32(&mut bytes, 0x400 + 28, 0x1122_3344);

    put_u32(&mut bytes, 0x420 + 4, 0x20b0);
    put_u32(&mut bytes, 0x420 + 8, 0x2010);
    put_u32(&mut bytes, 0x420 + 12, 0x2090);
    put_u32(&mut bytes, 0x420 + 16, 0x2080);
    // 0x440 保持全零终止符。

    put_u32(&mut bytes, 0x460, 0x20c0);
    put_u32(&mut bytes, 0x464, 0x8000_0005);
    put_u32(&mut bytes, 0x470, 0x1000);
    put_u32(&mut bytes, 0x474, 0x2000);
    put_u32(&mut bytes, 0x480, 0x20d0);
    put_u32(&mut bytes, 0x490, 0x3000);

    bytes[0x4a0..0x4aa].copy_from_slice(b"FIRST.DLL\0");
    bytes[0x4b0..0x4bb].copy_from_slice(b"SECOND.DLL\0");
    put_u16(&mut bytes, 0x4c0, 7);
    bytes[0x4c2..0x4ca].copy_from_slice(b"FuncOne\0");
    bytes[0x4d2..0x4d6].copy_from_slice(b"One\0");
    bytes
}

#[test]
fn pe_delay_imports_walk_descriptors_and_thunks() {
    let bytes = pe_delay_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_delay_imports(&image, &bytes).expect("PE delay-import directory");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.directory_rva, 0x2000);
    assert_eq!(report.total_modules, 2);
    assert_eq!(report.modules.len(), 2);
    assert_eq!(report.total_thunks, 3);

    let first = &report.modules[0];
    assert_eq!(first.name, "FIRST.DLL");
    assert_eq!(first.timestamp, 0x1122_3344);
    assert_eq!(first.module_handle_rva, 0x2008);
    assert_eq!(first.iat_rva, 0x2070);
    assert_eq!(first.int_rva, 0x2060);
    assert_eq!(first.thunks.len(), 2);
    let by_name = &first.thunks[0];
    assert_eq!(by_name.int_rva, 0x2060);
    assert_eq!(by_name.iat_rva, Some(0x2070));
    assert_eq!(by_name.name.as_deref(), Some("FuncOne"));
    assert_eq!(by_name.hint, Some(7));
    assert_eq!(by_name.ordinal, None);
    let by_ordinal = &first.thunks[1];
    assert_eq!(by_ordinal.int_rva, 0x2064);
    assert_eq!(by_ordinal.iat_rva, Some(0x2074));
    assert_eq!(by_ordinal.name, None);
    assert_eq!(by_ordinal.ordinal, Some(5));

    let second = &report.modules[1];
    assert_eq!(second.name, "SECOND.DLL");
    assert_eq!(second.timestamp, 0);
    assert_eq!(second.thunks.len(), 1);
    assert_eq!(second.thunks[0].name.as_deref(), Some("One"));
    assert_eq!(second.thunks[0].hint, Some(0));
    assert_eq!(second.thunks[0].iat_rva, Some(0x2090));
}

#[test]
fn pe_delay_imports_flag_unreadable_int_tables() {
    let mut bytes = pe_delay_bytes();
    put_u32(&mut bytes, 0x400 + 16, 0x9000);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_delay_imports(&image, &bytes).expect("PE delay-import directory");
    assert!(report.present);
    assert!(report.truncated);
    assert_eq!(report.modules.len(), 2);
    assert!(report.modules[0].thunks.is_empty());
    assert_eq!(report.modules[0].int_rva, 0x9000);
}

#[test]
fn pe_delay_imports_report_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_delay_imports(&image, &bytes).expect("PE delay-import directory");
    assert!(!report.present);
    assert_eq!(report.total_modules, 0);
    assert!(report.modules.is_empty());
}

/// `pe_bytes()` 加两个位于数据目录 11 之后的绑定导入描述符。
fn pe_bound_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 11 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 11 * 8 + 4, 48);

    put_u32(&mut bytes, 0x400, 0x1122_3344);
    put_u16(&mut bytes, 0x404, 0x40);
    put_u16(&mut bytes, 0x406, 2);
    put_u32(&mut bytes, 0x408, 0x5566_7788);
    put_u16(&mut bytes, 0x40c, 0x50);
    put_u32(&mut bytes, 0x410, 0);
    put_u16(&mut bytes, 0x414, 0x60);
    put_u32(&mut bytes, 0x418, 0x99aa_bbcc);
    put_u16(&mut bytes, 0x41c, 0x70);
    // 0x420 保持全零终止符。

    bytes[0x440..0x44a].copy_from_slice(b"BOUND.DLL\0");
    bytes[0x450..0x459].copy_from_slice(b"REF1.DLL\0");
    bytes[0x460..0x469].copy_from_slice(b"REF2.DLL\0");
    bytes[0x470..0x47b].copy_from_slice(b"SECOND.DLL\0");
    bytes
}

#[test]
fn pe_bound_imports_walk_descriptors_and_forwarders() {
    let bytes = pe_bound_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_bound_imports(&image, &bytes).expect("PE bound-import directory");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.directory_rva, 0x2000);
    assert_eq!(report.total_modules, 2);
    assert_eq!(report.modules.len(), 2);
    assert_eq!(report.total_forwarders, 2);

    let first = &report.modules[0];
    assert_eq!(first.timestamp, 0x1122_3344);
    assert_eq!(first.offset_module_name, 0x40);
    assert_eq!(first.name, "BOUND.DLL");
    assert_eq!(first.forwarders.len(), 2);
    assert_eq!(first.forwarders[0].timestamp, 0x5566_7788);
    assert_eq!(first.forwarders[0].offset_module_name, 0x50);
    assert_eq!(first.forwarders[0].name, "REF1.DLL");
    assert_eq!(first.forwarders[1].timestamp, 0);
    assert_eq!(first.forwarders[1].name, "REF2.DLL");

    let second = &report.modules[1];
    assert_eq!(second.timestamp, 0x99aa_bbcc);
    assert_eq!(second.offset_module_name, 0x70);
    assert_eq!(second.name, "SECOND.DLL");
    assert!(second.forwarders.is_empty());
}

#[test]
fn pe_bound_imports_flag_unresolvable_names() {
    let mut bytes = pe_bound_bytes();
    put_u16(&mut bytes, 0x404, 0xffff);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_bound_imports(&image, &bytes).expect("PE bound-import directory");
    assert!(report.present);
    assert!(report.truncated);
    assert_eq!(report.modules.len(), 2);
    assert_eq!(report.modules[0].name, "");
    assert_eq!(report.modules[1].name, "SECOND.DLL");
}

#[test]
fn pe_bound_imports_report_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_bound_imports(&image, &bytes).expect("PE bound-import directory");
    assert!(!report.present);
    assert_eq!(report.total_modules, 0);
    assert!(report.modules.is_empty());
}

/// `pe_bytes()` 加一个位于 DOS 桩区域的两条目 Rich 头。
fn pe_rich_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    let key = 0x7788_99AAu32;
    put_u32(&mut bytes, 0x40, 0x536e_6144 ^ key);
    put_u32(&mut bytes, 0x50, ((30729u32 << 16) | 155) ^ key);
    put_u32(&mut bytes, 0x54, 3 ^ key);
    put_u32(&mut bytes, 0x58, ((1u32 << 16) | 216) ^ key);
    put_u32(&mut bytes, 0x5c, 2 ^ key);
    bytes[0x60..0x64].copy_from_slice(b"Rich");
    put_u32(&mut bytes, 0x64, key);
    bytes
}

#[test]
fn pe_rich_header_decodes_xor_entries() {
    let bytes = pe_rich_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_rich_header(&image, &bytes).expect("PE Rich header");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.key, 0x7788_99AA);
    assert_eq!(report.dans_offset, 0x40);
    assert_eq!(report.rich_offset, 0x60);
    assert_eq!(report.total, 2);
    assert_eq!(report.entries.len(), 2);

    let first = &report.entries[0];
    assert_eq!(first.comp_id, 155);
    assert_eq!(first.build, 30729);
    assert_eq!(first.count, 3);
    assert_eq!(first.name.as_deref(), Some("Microsoft Visual C++ 14.x (VS2017-2022)"));

    let second = &report.entries[1];
    assert_eq!(second.comp_id, 216);
    assert_eq!(second.build, 1);
    assert_eq!(second.count, 2);
    assert_eq!(second.name.as_deref(), Some("Microsoft C/C++ linker or CRT (VS2017-2022 era)"));
}

#[test]
fn pe_rich_header_flags_malformed_runs() {
    let mut bytes = pe_rich_bytes();
    bytes.copy_within(0x60..0x68, 0x64);
    bytes[0x60] = 0;
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_rich_header(&image, &bytes).expect("PE Rich header");
    assert!(report.present);
    assert!(report.truncated);
    assert_eq!(report.total, 2);
    assert_eq!(report.entries.len(), 2);

    let mut bytes = pe_rich_bytes();
    put_u32(&mut bytes, 0x40, 0);
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_rich_header(&image, &bytes).expect("PE Rich header");
    assert!(!report.present);
}

#[test]
fn pe_rich_header_reports_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_rich_header(&image, &bytes).expect("PE Rich header");
    assert!(!report.present);
    assert_eq!(report.total, 0);
    assert!(report.entries.is_empty());
}

#[test]
fn pe_certificates_decode_entries_and_pkcs7_shape() {
    let mut bytes = pe_bytes();
    let (offset, size) = (0x500u32, 0x18u32);
    let directory = 0x98 + 112 + 4 * 8;
    put_u32(&mut bytes, directory, offset);
    put_u32(&mut bytes, directory + 4, size);

    put_u32(&mut bytes, 0x500, 12);
    put_u16(&mut bytes, 0x504, 0x0200);
    put_u16(&mut bytes, 0x506, 2);
    bytes[0x508..0x50c].copy_from_slice(&[0x30, 0x82, 0x00, 0x10]);

    put_u32(&mut bytes, 0x510, 8);
    put_u16(&mut bytes, 0x514, 0x0100);
    put_u16(&mut bytes, 0x516, 1);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_certificates(&image, &bytes).expect("PE certificates");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.total, 2);
    assert_eq!(report.directory_size, 0x18);
    assert_eq!(report.certificates.len(), 2);

    assert_eq!(report.certificates[0].offset, 0x500);
    assert_eq!(report.certificates[0].length, 12);
    assert_eq!(report.certificates[0].revision, "2.0");
    assert_eq!(report.certificates[0].certificate_type, "PKCS_SIGNED_DATA");
    assert_eq!(report.certificates[0].payload_size, 4);
    assert!(report.certificates[0].pkcs7);

    assert_eq!(report.certificates[1].offset, 0x510);
    assert_eq!(report.certificates[1].revision, "1.0");
    assert_eq!(report.certificates[1].certificate_type, "X509");
    assert_eq!(report.certificates[1].payload_size, 0);
    assert!(!report.certificates[1].pkcs7);
}

#[test]
fn pe_certificates_report_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_certificates(&image, &bytes).expect("PE certificates");
    assert!(!report.present);
    assert_eq!(report.total, 0);
    assert!(report.certificates.is_empty());
}

/// `pe_bytes()` 加一个位于 RVA 0x2000 的 PE32+ TLS 目录与两个回调。
fn pe_tls_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 9 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 9 * 8 + 4, 40);

    let base = 0x400;
    let image_base = 0x1_4000_0000u64;
    put_u64(&mut bytes, base, image_base + 0x2010);
    put_u64(&mut bytes, base + 8, image_base + 0x2020);
    put_u64(&mut bytes, base + 16, image_base + 0x2030);
    put_u64(&mut bytes, base + 24, image_base + 0x2040);
    put_u32(&mut bytes, base + 32, 0x10);
    put_u32(&mut bytes, base + 36, 0x0030_0000);

    put_u64(&mut bytes, 0x440, image_base + 0x1010);
    put_u64(&mut bytes, 0x448, image_base + 0x1020);
    put_u64(&mut bytes, 0x450, 0);
    bytes
}

#[test]
fn pe_tls_decodes_callbacks_and_maps_them_through_the_image_base() {
    let bytes = pe_tls_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_tls_directory(&image, &bytes).expect("PE TLS");

    assert!(report.present);
    assert!(report.is64);
    assert!(!report.truncated);
    assert_eq!(report.directory_rva, 0x2000);
    assert_eq!(report.directory_size, 40);
    assert_eq!(report.raw_data_start, 0x1_4000_2010);
    assert_eq!(report.raw_data_end, 0x1_4000_2020);
    assert_eq!(report.index_address, 0x1_4000_2030);
    assert_eq!(report.callbacks_address, 0x1_4000_2040);
    assert_eq!(report.callbacks_rva, Some(0x2040));
    assert_eq!(report.size_of_zero_fill, 0x10);
    assert_eq!(report.characteristics, 0x0030_0000);

    assert_eq!(report.callbacks.len(), 2);
    assert_eq!(report.callbacks[0].address, 0x1_4000_1010);
    assert_eq!(report.callbacks[0].rva, Some(0x1010));
    assert_eq!(report.callbacks[0].offset, Some(0x210));
    assert_eq!(report.callbacks[1].rva, Some(0x1020));
    assert_eq!(report.callbacks[1].offset, Some(0x220));
}

#[test]
fn pe_tls_reports_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_tls_directory(&image, &bytes).expect("PE TLS");
    assert!(!report.present);
    assert!(report.callbacks.is_empty());
}

#[test]
fn pe_load_config_decodes_guard_metadata() {
    let bytes = pe_load_config_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_load_config(&image, &bytes).expect("PE load configuration");

    assert!(report.present);
    assert!(report.is64);
    assert!(!report.truncated);
    assert_eq!(report.size, 0xa0);
    assert_eq!(report.timestamp, 0x0102_0304);
    assert_eq!(report.global_flags_clear, 1);
    assert_eq!(report.global_flags_set, 2);
    assert_eq!(report.critical_section_default_timeout, 1000);
    assert_eq!(report.security_cookie, 0x1_4000_2010);
    assert_eq!(report.se_handler_table, 0x1_4000_2020);
    assert_eq!(report.se_handler_count, 2);
    assert_eq!(report.guard_cf_function_table, 0x1_4000_2040);
    assert_eq!(report.guard_cf_function_count, 4);
    assert_eq!(report.guard_flags, 0x0010_0500);
    assert_eq!(report.guard_flags_text, vec!["CF_INSTRUMENTED", "CF_FUNCTION_TABLE_PRESENT", "RETPOLINE_PRESENT"]);
    assert_eq!(report.code_integrity.flags, 1);
    assert_eq!(report.code_integrity.catalog, 0x1234);
    assert_eq!(report.code_integrity.catalog_offset, 0x800);
}

#[test]
fn pe_load_config_reports_absence() {
    let bytes = pe_bytes();
    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_load_config(&image, &bytes).expect("PE load configuration");
    assert!(!report.present);
    assert!(report.guard_flags_text.is_empty());
}

/// `pe_bytes()` 加一个位于 RVA 0x2000 的 PE32+ 加载配置表。
fn pe_load_config_bytes() -> Vec<u8> {
    let mut bytes = pe_bytes();
    put_u32(&mut bytes, 0x98 + 112 + 10 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 10 * 8 + 4, 0xa0);

    let base = 0x400;
    let image_base = 0x1_4000_0000u64;
    put_u32(&mut bytes, base, 0xa0);
    put_u32(&mut bytes, base + 4, 0x0102_0304);
    put_u16(&mut bytes, base + 8, 1);
    put_u32(&mut bytes, base + 0x0c, 1);
    put_u32(&mut bytes, base + 0x10, 2);
    put_u32(&mut bytes, base + 0x14, 1000);
    put_u64(&mut bytes, base + 0x18, 0x1000);
    put_u64(&mut bytes, base + 0x20, 0x1_0000);
    put_u64(&mut bytes, base + 0x28, image_base + 0x1010);
    put_u64(&mut bytes, base + 0x30, 0x10_0000);
    put_u64(&mut bytes, base + 0x38, 0x20_0000);
    put_u64(&mut bytes, base + 0x40, 3);
    put_u32(&mut bytes, base + 0x48, 2);
    put_u64(&mut bytes, base + 0x58, image_base + 0x2010);
    put_u64(&mut bytes, base + 0x60, image_base + 0x2020);
    put_u64(&mut bytes, base + 0x68, 2);
    put_u64(&mut bytes, base + 0x70, image_base + 0x1030);
    put_u64(&mut bytes, base + 0x80, image_base + 0x2040);
    put_u64(&mut bytes, base + 0x88, 4);
    put_u32(&mut bytes, base + 0x90, 0x0010_0500);
    put_u16(&mut bytes, base + 0x94, 1);
    put_u16(&mut bytes, base + 0x96, 0x1234);
    put_u32(&mut bytes, base + 0x98, 0x800);
    bytes
}

#[test]
fn pe_mitigations_read_the_optional_header() {
    let mut bytes = pe_bytes();
    put_u16(&mut bytes, 0x98 + 0x46, 0x0160);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_mitigations(&image, &bytes).expect("PE mitigations");

    assert_eq!(report.dll_characteristics, 0x0160);
    assert!(report.aslr);
    assert!(report.dep);
    assert!(report.high_entropy_va);
    assert!(!report.cfg);
    assert!(!report.force_integrity);
    assert!(!report.no_seh);
    assert!(!report.relocations_present);
    assert!(!report.signed);
    assert!(report.writable_executable_sections.is_empty());
    assert_eq!(report.security_cookie, None);
    assert_eq!(report.guard_flags, None);
}

#[test]
fn pe_mitigations_detect_wx_relocations_and_load_config() {
    let mut bytes = pe_bytes();
    bytes.resize(0x800, 0);
    put_u16(&mut bytes, 0x98 + 0x46, 0x0000);
    put_u32(&mut bytes, 0x188 + 36, 0xe000_0020);

    put_u32(&mut bytes, 0x98 + 112 + 5 * 8, 0x1000);
    put_u32(&mut bytes, 0x98 + 112 + 5 * 8 + 4, 12);
    put_u32(&mut bytes, 0x98 + 112 + 4 * 8, 0x700);
    put_u32(&mut bytes, 0x98 + 112 + 4 * 8 + 4, 8);
    put_u32(&mut bytes, 0x98 + 112 + 10 * 8, 0x2000);
    put_u32(&mut bytes, 0x98 + 112 + 10 * 8 + 4, 0x94);

    put_u32(&mut bytes, 0x400, 0x94);
    put_u64(&mut bytes, 0x400 + 0x58, 0x1_4000_1000);
    put_u32(&mut bytes, 0x400 + 0x90, 0x100);

    let image = parse_pe(&bytes).expect("parse PE");
    let report = read_mitigations(&image, &bytes).expect("PE mitigations");

    assert!(!report.aslr);
    assert!(!report.dep);
    assert!(report.cfg);
    assert!(report.relocations_present);
    assert!(report.signed);
    assert_eq!(report.writable_executable_sections.len(), 1);
    assert_eq!(report.writable_executable_sections[0].name, ".text");
    assert_eq!(report.security_cookie, Some(true));
    assert_eq!(report.guard_flags, Some(0x100));
}
