//! Mach-O 身份、fat 切片、导出 trie、加密、链式修复、函数起始、代码签名与绑定。

use data_binary_mach_o::mach_o::{
    MachBindType, MachDylibKind, MachPlatform, parse_macho, read_bindings, read_chained_fixups, read_code_signature,
    read_exports, read_fat_slices, read_function_starts,
};

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_name16(bytes: &mut [u8], offset: usize, name: &[u8]) {
    let length = name.len().min(16);
    bytes[offset..offset + length].copy_from_slice(&name[..length]);
}

fn put_be_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put_be_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

/// 瘦 arm64 镜像：`__TEXT,__text`、`LC_MAIN`、`LC_UUID`、一条 dylib 与
/// `LC_BUILD_VERSION`。代码载荷从文件偏移 0x200 开始。
fn macho64_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x210];
    bytes[0..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    put_u32(&mut bytes, 4, 0x0100_000c);
    put_u32(&mut bytes, 8, 0);
    put_u32(&mut bytes, 12, 2);
    put_u32(&mut bytes, 16, 5);
    put_u32(&mut bytes, 20, 280);
    put_u32(&mut bytes, 24, 0x0020_0085);

    let segment = 32;
    put_u32(&mut bytes, segment, 0x19);
    put_u32(&mut bytes, segment + 4, 152);
    put_name16(&mut bytes, segment + 8, b"__TEXT");
    put_u64(&mut bytes, segment + 24, 0x1000);
    put_u64(&mut bytes, segment + 32, 0x200);
    put_u64(&mut bytes, segment + 40, 0x200);
    put_u64(&mut bytes, segment + 48, 0x10);
    put_u32(&mut bytes, segment + 56, 5);
    put_u32(&mut bytes, segment + 60, 5);
    put_u32(&mut bytes, segment + 64, 1);

    let section = segment + 72;
    put_name16(&mut bytes, section, b"__text");
    put_name16(&mut bytes, section + 16, b"__TEXT");
    put_u64(&mut bytes, section + 32, 0x1000);
    put_u64(&mut bytes, section + 40, 0x10);
    put_u32(&mut bytes, section + 48, 0x200);
    put_u32(&mut bytes, section + 52, 2);
    put_u32(&mut bytes, section + 64, 0x8000_0400);

    let main = segment + 152;
    put_u32(&mut bytes, main, 0x8000_0028);
    put_u32(&mut bytes, main + 4, 24);
    put_u64(&mut bytes, main + 8, 0x200);

    let uuid = main + 24;
    put_u32(&mut bytes, uuid, 0x1b);
    put_u32(&mut bytes, uuid + 4, 24);
    bytes[uuid + 8..uuid + 24]
        .copy_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);

    let dylib = uuid + 24;
    put_u32(&mut bytes, dylib, 0x0c);
    put_u32(&mut bytes, dylib + 4, 56);
    put_u32(&mut bytes, dylib + 8, 24);
    put_u32(&mut bytes, dylib + 12, 2);
    put_u32(&mut bytes, dylib + 16, 0x0001_0203);
    put_u32(&mut bytes, dylib + 20, 0x0001_0000);
    bytes[dylib + 24..dylib + 51].copy_from_slice(b"/usr/lib/libSystem.B.dylib\0");

    let build = dylib + 56;
    put_u32(&mut bytes, build, 0x32);
    put_u32(&mut bytes, build + 4, 24);
    put_u32(&mut bytes, build + 8, 1);
    put_u32(&mut bytes, build + 12, 0x000d_0100);
    put_u32(&mut bytes, build + 16, 0x000e_0000);

    bytes[0x200..0x210].copy_from_slice(&[0xc0, 0x03, 0x5f, 0xd6, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
    bytes
}

#[test]
fn macho_identity_reports_uuid_build_version_and_dylibs() {
    let image = parse_macho(&macho64_bytes()).expect("Mach-O image");

    assert!(image.header.is_64);
    assert_eq!(image.uuid_string().as_deref(), Some("00112233-4455-6677-8899-aabbccddeeff"));
    let build = image.build.expect("LC_BUILD_VERSION");
    assert_eq!(build.platform, MachPlatform::MacOs);
    assert_eq!(build.platform.as_str(), "macos");
    assert_eq!(build.min_os_version(), "13.1.0");
    assert_eq!(build.sdk_version(), "14.0.0");
    assert_eq!(build.min_os, 0x000d_0100);

    assert_eq!(image.dylibs.len(), 1);
    assert_eq!(image.dylibs[0].kind, MachDylibKind::Load);
    assert_eq!(image.dylibs[0].name, "/usr/lib/libSystem.B.dylib");
    assert_eq!(image.dylibs[0].timestamp, 2);
    assert!(!image.dylibs[0].is_lazy());
    assert_eq!(image.dylibs[0].current_version, 0x0001_0203);
    assert_eq!(image.dylibs[0].ordinal, Some(1));
    assert_eq!(image.entry_offset, Some(0x200));
}

#[test]
fn macho_lazy_dylib_keeps_ordinal_and_lazy_flag() {
    let mut bytes = macho64_bytes();
    put_u32(&mut bytes, 232, 0x20);
    let image = parse_macho(&bytes).expect("Mach-O image");

    assert_eq!(image.dylibs[0].kind, MachDylibKind::Lazy);
    assert!(image.dylibs[0].is_lazy());
    assert_eq!(image.dylibs[0].kind.as_str(), "lazy");
    assert_eq!(image.dylibs[0].ordinal, Some(1));
}

/// [`macho64_bytes`] 加 `LC_FUNCTION_STARTS`，增量 0x100、0x20、0x40 与终止零。
fn macho64_function_starts_bytes() -> Vec<u8> {
    let mut bytes = macho64_bytes();
    bytes.resize(0x320, 0);
    put_u32(&mut bytes, 16, 6);
    put_u32(&mut bytes, 20, 296);

    let command = 32 + 280;
    put_u32(&mut bytes, command, 0x26);
    put_u32(&mut bytes, command + 4, 16);
    put_u32(&mut bytes, command + 8, 0x300);
    put_u32(&mut bytes, command + 12, 8);

    bytes[0x300] = 0x80;
    bytes[0x301] = 0x02;
    bytes[0x302] = 0x20;
    bytes[0x303] = 0x40;
    bytes[0x304] = 0x00;
    bytes
}

#[test]
fn macho_function_starts_decode_uleb128_deltas() {
    let bytes = macho64_function_starts_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_function_starts(&bytes, &image, None).expect("Mach-O function starts");

    assert_eq!(report.image_base, 0x1000);
    assert_eq!(report.total, 3);
    assert!(!report.truncated);
    assert_eq!(report.addresses, vec![0x1100, 0x1120, 0x1160]);

    let bounded = read_function_starts(&bytes, &image, Some(2)).expect("bounded function starts");
    assert_eq!(bounded.total, 3);
    assert!(bounded.truncated);
    assert_eq!(bounded.addresses, vec![0x1100, 0x1120]);
}

#[test]
fn macho_function_starts_require_the_command() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let error = read_function_starts(&bytes, &image, None).expect_err("no LC_FUNCTION_STARTS");
    assert_eq!(error.code(), "macho/function-starts-missing");
}

/// [`macho64_bytes`] 加 `LC_DYLD_EXPORTS_TRIE`，导出 `_foo` 与弱 `_bar`。
fn macho64_exports_bytes() -> Vec<u8> {
    let mut bytes = macho64_bytes();
    bytes.resize(0x320, 0);
    put_u32(&mut bytes, 16, 6);
    put_u32(&mut bytes, 20, 296);

    let command = 32 + 280;
    put_u32(&mut bytes, command, 0x8000_0033);
    put_u32(&mut bytes, command + 4, 16);
    put_u32(&mut bytes, command + 8, 0x300);
    put_u32(&mut bytes, command + 12, 24);

    let trie: [u8; 24] = [
        0x00, 0x02, b'_', b'f', b'o', b'o', 0x00, 0x0e, b'_', b'b', b'a', b'r', 0x00, 0x13, 0x03, 0x00, 0x80, 0x20, 0x00, 0x03,
        0x01, 0xc0, 0x20, 0x00,
    ];
    bytes[0x300..0x300 + trie.len()].copy_from_slice(&trie);
    bytes
}

#[test]
fn macho_exports_walk_the_dyld_trie() {
    let bytes = macho64_exports_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_exports(&bytes, &image);

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.total, 2);
    assert_eq!(report.exports.len(), 2);

    let first = &report.exports[0];
    assert_eq!(first.name, "_foo");
    assert_eq!(first.flags, 0);
    assert!(first.flag_names.is_empty());
    assert_eq!(first.address, Some(0x1000));
    assert_eq!(first.other, None);

    let second = &report.exports[1];
    assert_eq!(second.name, "_bar");
    assert_eq!(second.flags, 1);
    assert_eq!(second.flag_names, vec!["WEAK"]);
    assert_eq!(second.address, Some(0x1040));
}

#[test]
fn macho_exports_report_absence() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_exports(&bytes, &image);
    assert!(!report.present);
    assert!(report.exports.is_empty());
}

/// [`macho64_bytes`] 加 `LC_ENCRYPTION_INFO_64`，加密区间 0x2000+0x1000。
fn macho64_encryption_bytes() -> Vec<u8> {
    let mut bytes = macho64_bytes();
    put_u32(&mut bytes, 16, 6);
    put_u32(&mut bytes, 20, 304);

    let command = 32 + 280;
    put_u32(&mut bytes, command, 0x2c);
    put_u32(&mut bytes, command + 4, 24);
    put_u32(&mut bytes, command + 8, 0x2000);
    put_u32(&mut bytes, command + 12, 0x1000);
    put_u32(&mut bytes, command + 16, 1);
    put_u32(&mut bytes, command + 20, 0);
    bytes
}

#[test]
fn macho_encryption_reports_the_crypt_range() {
    let image = parse_macho(&macho64_encryption_bytes()).expect("Mach-O image");
    let encryption = image.encryption.expect("LC_ENCRYPTION_INFO_64");

    assert!(encryption.is_64);
    assert_eq!(encryption.crypt_offset, 0x2000);
    assert_eq!(encryption.crypt_size, 0x1000);
    assert_eq!(encryption.crypt_id, 1);
    assert!(encryption.encrypted());
}

#[test]
fn macho_encryption_reports_absence() {
    let image = parse_macho(&macho64_bytes()).expect("Mach-O image");
    assert!(image.encryption.is_none());
}

/// [`macho64_bytes`] 加 `LC_DYLD_CHAINED_FIXUPS`：2 条导入与 2 段 starts 表。
fn macho64_chained_fixups_bytes() -> Vec<u8> {
    let mut bytes = macho64_bytes();
    bytes.resize(0x400, 0);
    put_u32(&mut bytes, 16, 6);
    put_u32(&mut bytes, 20, 296);

    let command = 32 + 280;
    put_u32(&mut bytes, command, 0x8000_0034);
    put_u32(&mut bytes, command + 4, 16);
    put_u32(&mut bytes, command + 8, 0x300);
    put_u32(&mut bytes, command + 12, 0x80);

    put_u32(&mut bytes, 0x300, 0);
    put_u32(&mut bytes, 0x304, 0x30);
    put_u32(&mut bytes, 0x308, 0x1c);
    put_u32(&mut bytes, 0x30c, 0x24);
    put_u32(&mut bytes, 0x310, 2);
    put_u32(&mut bytes, 0x314, 1);
    put_u32(&mut bytes, 0x318, 0);

    put_u32(&mut bytes, 0x31c, 1);
    put_u32(&mut bytes, 0x320, 2 | (1 << 8) | (5 << 9));
    bytes[0x324..0x324 + 5].copy_from_slice(b"puts\0");
    bytes[0x329..0x329 + 7].copy_from_slice(b"malloc\0");

    put_u32(&mut bytes, 0x330, 2);
    put_u32(&mut bytes, 0x334, 0x0c);
    put_u32(&mut bytes, 0x338, 0x18);
    bytes
}

#[test]
fn macho_chained_fixups_report_imports_and_starts() {
    let bytes = macho64_chained_fixups_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_chained_fixups(&bytes, &image);

    assert!(report.present);
    assert_eq!(report.data_offset, 0x300);
    assert_eq!(report.data_size, 0x80);
    let header = report.header.expect("fixups header");
    assert_eq!(header.fixups_version, 0);
    assert_eq!(header.starts_offset, 0x30);
    assert_eq!(header.imports_offset, 0x1c);
    assert_eq!(header.symbols_offset, 0x24);
    assert_eq!(header.imports_count, 2);
    assert_eq!(header.imports_format, 1);
    assert_eq!(header.symbols_format, 0);

    assert_eq!(report.imports.len(), 2);
    assert_eq!(report.imports[0].lib_ordinal, 1);
    assert!(!report.imports[0].weak_import);
    assert_eq!(report.imports[0].name, "puts");
    assert_eq!(report.imports[0].addend, None);
    assert_eq!(report.imports[1].lib_ordinal, 2);
    assert!(report.imports[1].weak_import);
    assert_eq!(report.imports[1].name, "malloc");
    assert!(!report.truncated);

    assert_eq!(report.starts_in_image, Some(2));
    assert_eq!(report.starts_in_segment_count, 2);
}

#[test]
fn macho_chained_fixups_report_absence() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_chained_fixups(&bytes, &image);
    assert!(!report.present);
    assert!(report.imports.is_empty());
}

/// [`macho64_bytes`] 加第二条 `LC_LOAD_DYLIB`（`libz`）与 `LC_DYLD_INFO_ONLY`，
/// 三条绑定流共 6 条绑定。
fn macho64_bindings_bytes() -> Vec<u8> {
    let mut bytes = macho64_bytes();
    bytes.resize(0x600, 0);
    put_u32(&mut bytes, 16, 7);
    put_u32(&mut bytes, 20, 384);

    let dylib = 32 + 280;
    put_u32(&mut bytes, dylib, 0x0c);
    put_u32(&mut bytes, dylib + 4, 56);
    put_u32(&mut bytes, dylib + 8, 24);
    put_u32(&mut bytes, dylib + 12, 2);
    put_u32(&mut bytes, dylib + 16, 0x0001_0203);
    put_u32(&mut bytes, dylib + 20, 0x0001_0000);
    bytes[dylib + 24..dylib + 46].copy_from_slice(b"/usr/lib/libz.1.dylib\0");

    let info = dylib + 56;
    put_u32(&mut bytes, info, 0x8000_0022);
    put_u32(&mut bytes, info + 4, 48);
    put_u32(&mut bytes, info + 8, 0);
    put_u32(&mut bytes, info + 12, 0);
    put_u32(&mut bytes, info + 16, 0x400);
    put_u32(&mut bytes, info + 20, 37);
    put_u32(&mut bytes, info + 24, 0x480);
    put_u32(&mut bytes, info + 28, 15);
    put_u32(&mut bytes, info + 32, 0x4c0);
    put_u32(&mut bytes, info + 36, 17);
    put_u32(&mut bytes, info + 40, 0);
    put_u32(&mut bytes, info + 44, 0);

    bytes[0x400] = 0x11;
    bytes[0x401] = 0x51;
    bytes[0x402] = 0x70;
    bytes[0x403] = 0x20;
    bytes[0x404] = 0x40;
    bytes[0x405..0x417].copy_from_slice(b"___stack_chk_fail\0");
    bytes[0x417] = 0x90;
    bytes[0x418] = 0x80;
    bytes[0x419] = 0x18;
    bytes[0x41a] = 0x62;
    bytes[0x41b] = 0x02;
    bytes[0x41c] = 0x41;
    bytes[0x41d..0x422].copy_from_slice(b"_bar\0");
    bytes[0x422] = 0xa0;
    bytes[0x423] = 0x08;
    bytes[0x424] = 0x00;

    bytes[0x480] = 0x11;
    bytes[0x481] = 0x52;
    bytes[0x482] = 0x70;
    bytes[0x483] = 0x00;
    bytes[0x484] = 0x42;
    bytes[0x485..0x48c].copy_from_slice(b"_wmain\0");
    bytes[0x48c] = 0x90;
    bytes[0x48d] = 0x00;

    bytes[0x4c0] = 0x20;
    bytes[0x4c1] = 0x02;
    bytes[0x4c2] = 0x51;
    bytes[0x4c3] = 0x40;
    bytes[0x4c4..0x4cc].copy_from_slice(b"_lazy_a\0");
    bytes[0x4cc] = 0xb0;
    bytes[0x4cd] = 0xc0;
    bytes[0x4ce] = 0x02;
    bytes[0x4cf] = 0x01;
    bytes[0x4d0] = 0x00;
    bytes
}

#[test]
fn macho_bindings_decode_classic_bind_opcodes() {
    let bytes = macho64_bindings_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let bindings = read_bindings(&bytes, &image).expect("Mach-O dyld bind opcodes");

    assert_eq!(bindings.len(), 6);

    let first = &bindings[0];
    assert_eq!(first.stream, "bind");
    assert_eq!(first.stream_name(), "bind");
    assert_eq!(first.address, 0x1020);
    assert_eq!(first.segment, "__TEXT");
    assert_eq!(first.symbol, "___stack_chk_fail");
    assert_eq!(first.library.as_deref(), Some("/usr/lib/libSystem.B.dylib"));
    assert_eq!(first.dylib_name(), "/usr/lib/libSystem.B.dylib");
    assert_eq!(first.bind_type, MachBindType::Pointer);
    assert_eq!(first.bind_type.name(), "pointer");
    assert!(!first.weak);
    assert_eq!(first.addend, 0);

    let second = &bindings[1];
    assert_eq!(second.address, 0x1040);
    assert_eq!(second.symbol, "_bar");
    assert!(second.weak);
    assert_eq!(second.addend, 2);

    let third = &bindings[2];
    assert_eq!(third.stream, "weak_bind");
    assert_eq!(third.stream_name(), "weak");
    assert_eq!(third.address, 0x1000);
    assert_eq!(third.bind_type, MachBindType::TextAbsolute32);
    assert_eq!(third.library.as_deref(), Some("/usr/lib/libSystem.B.dylib"));

    let fourth = &bindings[3];
    assert_eq!(fourth.stream, "lazy_bind");
    assert_eq!(fourth.stream_name(), "lazy");
    assert_eq!(fourth.address, 0);
    assert_eq!(fourth.segment, "");
    assert_eq!(fourth.dylib_name(), "/usr/lib/libz.1.dylib");

    assert_eq!(bindings[4].address, 8);
    assert_eq!(bindings[4].segment, "");

    let last = &bindings[5];
    assert_eq!(last.stream_name(), "lazy");
    assert_eq!(last.address, 17);
    assert_eq!(last.symbol, "_lazy_a");
    assert_eq!(last.library.as_deref(), Some("/usr/lib/libz.1.dylib"));
    assert_eq!(last.bind_type, MachBindType::Pointer);
    assert!(!last.weak);
}

#[test]
fn macho_bindings_report_absence() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    assert!(read_bindings(&bytes, &image).expect("bindings").is_empty());
}

/// `fat_arch` 容器：x86_64 与 arm64 两个切片。
fn fat32_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x2400];
    put_be_u32(&mut bytes, 0, 0xcafe_babe);
    put_be_u32(&mut bytes, 4, 2);

    put_be_u32(&mut bytes, 8, 0x0100_0007);
    put_be_u32(&mut bytes, 12, 3);
    put_be_u32(&mut bytes, 16, 0x1000);
    put_be_u32(&mut bytes, 20, 0x200);
    put_be_u32(&mut bytes, 24, 12);

    put_be_u32(&mut bytes, 28, 0x0100_000c);
    put_be_u32(&mut bytes, 32, 0);
    put_be_u32(&mut bytes, 36, 0x2000);
    put_be_u32(&mut bytes, 40, 0x300);
    put_be_u32(&mut bytes, 44, 14);

    bytes[0x1000..0x1004].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes[0x2000..0x2004].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes
}

/// `fat_arch_64` 容器：一个 arm64 切片，64 位区间。
fn fat64_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x1200];
    put_be_u32(&mut bytes, 0, 0xcafe_babf);
    put_be_u32(&mut bytes, 4, 1);
    put_be_u32(&mut bytes, 8, 0x0100_000c);
    put_be_u32(&mut bytes, 12, 2);
    put_be_u64(&mut bytes, 16, 0x1000);
    put_be_u64(&mut bytes, 24, 0x100);
    put_be_u32(&mut bytes, 32, 12);
    put_be_u32(&mut bytes, 36, 0);
    bytes[0x1000..0x1004].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes
}

#[test]
fn macho_fat_slices_report_architectures_and_ranges() {
    let report = read_fat_slices(&fat32_bytes()).expect("fat slices");

    assert!(!report.is_fat64);
    assert_eq!(report.slices.len(), 2);
    assert_eq!(report.slices[0].index, 0);
    assert_eq!(report.slices[0].architecture.as_str(), "x64");
    assert_eq!(report.slices[0].cpu_type, 0x0100_0007);
    assert_eq!(report.slices[0].offset, 0x1000);
    assert_eq!(report.slices[0].size, 0x200);
    assert_eq!(report.slices[0].alignment, 12);
    assert_eq!(report.slices[0].bits, Some(64));

    assert_eq!(report.slices[1].architecture.as_str(), "arm64");
    assert_eq!(report.slices[1].offset, 0x2000);
    assert_eq!(report.slices[1].size, 0x300);
}

#[test]
fn macho_fat64_slices_use_64_bit_ranges() {
    let report = read_fat_slices(&fat64_bytes()).expect("fat64 slices");

    assert!(report.is_fat64);
    assert_eq!(report.slices.len(), 1);
    assert_eq!(report.slices[0].architecture.as_str(), "arm64");
    assert_eq!(report.slices[0].offset, 0x1000);
    assert_eq!(report.slices[0].size, 0x100);
    assert_eq!(report.slices[0].bits, Some(64));
}

#[test]
fn macho_fat_slice_outside_the_file_is_a_parse_error() {
    let mut bytes = fat32_bytes();
    put_be_u32(&mut bytes, 20, 0x4000);
    let error = read_fat_slices(&bytes).expect_err("out-of-file slice");
    assert_eq!(error.code(), "macho/fat-slice-out-of-range");
}

#[test]
fn macho_fat_slices_require_a_fat_container() {
    let error = read_fat_slices(&macho64_bytes()).expect_err("thin image");
    assert_eq!(error.code(), "macho/not-fat-binary");
}

/// Mach-O64，带一条 `LC_CODE_SIGNATURE` 与内嵌 `SuperBlob`。
fn macho_signature_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x400];
    bytes[0..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    put_u32(&mut bytes, 4, 0x0100_0007);
    put_u32(&mut bytes, 8, 3);
    put_u32(&mut bytes, 12, 2);
    put_u32(&mut bytes, 16, 1);
    put_u32(&mut bytes, 20, 16);
    put_u32(&mut bytes, 32, 0x1d);
    put_u32(&mut bytes, 36, 16);
    put_u32(&mut bytes, 40, 0x200);
    put_u32(&mut bytes, 44, 0x100);

    put_be_u32(&mut bytes, 0x200, 0xfade_0cc0);
    put_be_u32(&mut bytes, 0x204, 0x100);
    put_be_u32(&mut bytes, 0x208, 1);
    put_be_u32(&mut bytes, 0x20c, 0);
    put_be_u32(&mut bytes, 0x210, 0x20);

    let blob = 0x220;
    put_be_u32(&mut bytes, blob, 0xfade_0c02);
    put_be_u32(&mut bytes, blob + 4, 0x60);
    put_be_u32(&mut bytes, blob + 8, 0x20200);
    put_be_u32(&mut bytes, blob + 12, 2);
    put_be_u32(&mut bytes, blob + 20, 0x40);
    put_be_u32(&mut bytes, blob + 28, 2);
    put_be_u32(&mut bytes, blob + 32, 0x1000);
    bytes[blob + 36] = 32;
    bytes[blob + 37] = 2;
    bytes[blob + 39] = 12;
    put_be_u32(&mut bytes, blob + 48, 0x58);
    bytes[blob + 0x40..blob + 0x4f].copy_from_slice(b"com.example.app");
    bytes[blob + 0x4f] = 0;
    bytes[blob + 0x58..blob + 0x5d].copy_from_slice(b"TEAM\0");
    bytes
}

#[test]
fn macho_code_signature_decodes_the_code_directory() {
    let bytes = macho_signature_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_code_signature(&bytes, &image);

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.data_offset, 0x200);
    assert_eq!(report.data_size, 0x100);
    assert_eq!(report.magic.as_deref(), Some("embedded-signature"));
    assert_eq!(report.blob_count, Some(1));
    assert_eq!(report.blob_types, vec!["CodeDirectory"]);

    let code = report.code_directory.as_ref().expect("CodeDirectory");
    assert_eq!(code.version, 0x20200);
    assert_eq!(code.flags, 2);
    assert_eq!(code.flag_names, vec!["HARD"]);
    assert_eq!(code.hash_type, "SHA256");
    assert_eq!(code.hash_size, 32);
    assert_eq!(code.page_size, 4096);
    assert_eq!(code.code_limit, 0x1000);
    assert_eq!(code.identifier, "com.example.app");
    assert_eq!(code.team_id.as_deref(), Some("TEAM"));
    assert_eq!(code.code_slots, 2);
    assert_eq!(code.special_slots, 0);
}

#[test]
fn macho_code_signature_reports_absence() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("Mach-O image");
    let report = read_code_signature(&bytes, &image);
    assert!(!report.present);
    assert!(report.code_directory.is_none());
}
