//! `mach_o`：64 位 `Mach-O` 可执行镜像的构建与读取。

use data_binary_mach_o::mach_o::{
    MachArchitecture, MachDylibKind, MachOEndian, MachOImageBuilder, MachOParseError, architecture, parse_macho, parse_macho_header,
    read_bindings, read_macho_symbols,
};

const MH_MAGIC_64: u32 = 0xFEED_FACF;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_MAIN: u32 = 0x8000_0028;

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_name(bytes: &mut [u8], offset: usize, name: &[u8]) {
    let length = name.len().min(16);
    bytes[offset..offset + length].copy_from_slice(&name[..length]);
}

#[test]
fn mach_o_magic() {
    let exe = MachOImageBuilder::new().build_executable().unwrap();
    assert_eq!(&exe[..4], &MH_MAGIC_64.to_le_bytes());
}

#[test]
fn embeds_code_and_rodata() {
    let mut builder = MachOImageBuilder::new();
    builder.set_code(&[0xC0, 0x03, 0x5F, 0xD6]);
    builder.add_rodata(b"asgard");
    let exe = builder.build_executable().unwrap();
    assert_eq!(&exe[..4], &MH_MAGIC_64.to_le_bytes());
    assert!(exe.windows(b"asgard".len()).any(|window| window == b"asgard"));
}

#[test]
fn reads_builder_output() {
    let exe = MachOImageBuilder::new().build_executable().unwrap();
    let image = parse_macho(&exe).expect("parse builder output");

    assert!(image.header.is_64);
    assert_eq!(image.header.endian, MachOEndian::Little);
    assert_eq!(image.header.cpu_type, 0x0100_000C);
    assert_eq!(image.header.file_type, 2);
    assert_eq!(image.header.command_count, 2);
    assert_eq!(image.segments.len(), 1);
    assert_eq!(image.segments[0].name, "__TEXT");
    assert_eq!(image.segments[0].vm_address, 0x4000);
    assert!(image.sections.is_empty());
    assert_eq!(image.entry_offset, Some(128));

    let header = parse_macho_header(&exe).expect("parse header");
    assert_eq!(header.command_count, 2);
}

/// 64 位小端镜像：`__TEXT` 段带一个 `__text` 节，再加 `LC_MAIN`。
fn macho64_bytes() -> Vec<u8> {
    let segment_size = 72 + 80;
    let command_size = segment_size + 24;
    let mut bytes = vec![0u8; 32 + command_size];

    put_u32(&mut bytes, 0, MH_MAGIC_64);
    put_u32(&mut bytes, 4, 0x0100_000C);
    put_u32(&mut bytes, 8, 0);
    put_u32(&mut bytes, 12, 2);
    put_u32(&mut bytes, 16, 2);
    put_u32(&mut bytes, 20, command_size as u32);
    put_u32(&mut bytes, 24, 0x0020_0085);

    let segment = 32;
    put_u32(&mut bytes, segment, LC_SEGMENT_64);
    put_u32(&mut bytes, segment + 4, segment_size as u32);
    put_name(&mut bytes, segment + 8, b"__TEXT");
    put_u64(&mut bytes, segment + 24, 0x1_0000_0000);
    put_u64(&mut bytes, segment + 32, 0x4000);
    put_u64(&mut bytes, segment + 40, 0);
    put_u64(&mut bytes, segment + 48, 0x4000);
    put_u32(&mut bytes, segment + 56, 5);
    put_u32(&mut bytes, segment + 60, 5);
    put_u32(&mut bytes, segment + 64, 1);

    let section = segment + 72;
    put_name(&mut bytes, section, b"__text");
    put_name(&mut bytes, section + 16, b"__TEXT");
    put_u64(&mut bytes, section + 32, 0x1_0000_0400);
    put_u64(&mut bytes, section + 40, 0x10);
    put_u32(&mut bytes, section + 48, 0x400);
    put_u32(&mut bytes, section + 52, 4);
    put_u32(&mut bytes, section + 64, 0x8000_0400);

    let main = segment + segment_size;
    put_u32(&mut bytes, main, LC_MAIN);
    put_u32(&mut bytes, main + 4, 24);
    put_u64(&mut bytes, main + 8, 0x400);
    bytes
}

#[test]
fn reads_segments_sections_and_entry() {
    let image = parse_macho(&macho64_bytes()).expect("parse mach-o");

    assert_eq!(image.header.command_count, 2);
    assert_eq!(image.segments.len(), 1);
    assert_eq!(image.segments[0].name, "__TEXT");
    assert_eq!(image.segments[0].vm_address, 0x1_0000_0000);
    assert_eq!(image.segments[0].file_size, 0x4000);
    assert_eq!(image.segments[0].max_protection, 5);
    assert_eq!(image.segments[0].initial_protection, 5);

    assert_eq!(image.sections.len(), 1);
    assert_eq!(image.sections[0].name, "__text");
    assert_eq!(image.sections[0].segment, "__TEXT");
    assert_eq!(image.sections[0].address, 0x1_0000_0400);
    assert_eq!(image.sections[0].size, 0x10);
    assert_eq!(image.sections[0].file_offset, 0x400);
    assert_eq!(image.sections[0].alignment, 4);
    assert_eq!(image.sections[0].flags, 0x8000_0400);
    assert_eq!(image.entry_offset, Some(0x400));
}

#[test]
fn reads_big_endian_32_bit_header() {
    let mut bytes = vec![0u8; 28];
    bytes[0..4].copy_from_slice(&[0xfe, 0xed, 0xfa, 0xce]);
    bytes[4..8].copy_from_slice(&7i32.to_be_bytes());
    bytes[12..16].copy_from_slice(&2u32.to_be_bytes());
    bytes[16..20].copy_from_slice(&0u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&0u32.to_be_bytes());

    let header = parse_macho_header(&bytes).expect("parse big-endian mach-o");
    assert!(!header.is_64);
    assert_eq!(header.endian, MachOEndian::Big);
    assert_eq!(header.cpu_type, 7);
    assert_eq!(header.file_type, 2);

    let image = parse_macho(&bytes).expect("parse empty command list");
    assert!(image.segments.is_empty());
    assert_eq!(image.entry_offset, None);
}

#[test]
fn rejects_fat_and_truncated_input() {
    let mut fat = vec![0u8; 8];
    fat[0..4].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
    assert!(matches!(parse_macho_header(&fat), Err(MachOParseError::FatBinary)));

    assert!(matches!(parse_macho_header(b"MZ\x00\x00"), Err(MachOParseError::BadMagic)));

    let mut bytes = macho64_bytes();
    bytes.truncate(40);
    assert!(matches!(parse_macho(&bytes), Err(MachOParseError::OutOfRange { .. })));

    let mut bytes = macho64_bytes();
    put_u32(&mut bytes, 20, 4);
    assert!(matches!(parse_macho(&bytes), Err(MachOParseError::CommandOutOfRange { .. })));
}

/// 64 位小端镜像：`__TEXT` 段、`LC_LOAD_DYLIB`、`LC_DYLD_INFO_ONLY` 与两条绑定流。
fn macho64_with_bindings() -> Vec<u8> {
    const LC_LOAD_DYLIB: u32 = 0xC;
    const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
    let segment_size = 72u32;
    let dylib_size = 56u32;
    let dyld_info_size = 48u32;
    let command_size = segment_size + dylib_size + dyld_info_size;
    let mut bytes = vec![0u8; 0x400];

    put_u32(&mut bytes, 0, MH_MAGIC_64);
    put_u32(&mut bytes, 4, 0x0100_000C);
    put_u32(&mut bytes, 12, 2);
    put_u32(&mut bytes, 16, 3);
    put_u32(&mut bytes, 20, command_size);

    let segment = 32;
    put_u32(&mut bytes, segment, LC_SEGMENT_64);
    put_u32(&mut bytes, segment + 4, segment_size);
    put_name(&mut bytes, segment + 8, b"__TEXT");
    put_u64(&mut bytes, segment + 24, 0x1_0000_0000);
    put_u64(&mut bytes, segment + 32, 0x4000);
    put_u64(&mut bytes, segment + 40, 0);
    put_u64(&mut bytes, segment + 48, 0x400);
    put_u32(&mut bytes, segment + 56, 5);
    put_u32(&mut bytes, segment + 60, 5);

    let dylib = segment + segment_size as usize;
    let name = b"/usr/lib/libSystem.B.dylib\0";
    put_u32(&mut bytes, dylib, LC_LOAD_DYLIB);
    put_u32(&mut bytes, dylib + 4, dylib_size);
    put_u32(&mut bytes, dylib + 8, 24);
    put_u32(&mut bytes, dylib + 12, 2);
    put_u32(&mut bytes, dylib + 16, 0x1_0000);
    put_u32(&mut bytes, dylib + 20, 0x1_0000);
    bytes[dylib + 24..dylib + 24 + name.len()].copy_from_slice(name);

    let bind = [0x11, 0x41, b'_', b'p', b'u', b't', b's', 0, 0x70, 0x80, 0x02, 0x90, 0x00];
    let lazy = [
        0x11, 0x40, b'_', b'e', b'x', b'i', b't', 0, 0x70, 0x30, 0x90, 0x00, 0x11, 0x40, b'_', b'a', b'b', b'o', b'r', b't', 0, 0x70, 0x40,
        0x90, 0x00,
    ];
    bytes[0x300..0x300 + bind.len()].copy_from_slice(&bind);
    bytes[0x320..0x320 + lazy.len()].copy_from_slice(&lazy);

    let info = dylib + dylib_size as usize;
    put_u32(&mut bytes, info, LC_DYLD_INFO_ONLY);
    put_u32(&mut bytes, info + 4, dyld_info_size);
    put_u32(&mut bytes, info + 16, 0x300);
    put_u32(&mut bytes, info + 20, bind.len() as u32);
    put_u32(&mut bytes, info + 32, 0x320);
    put_u32(&mut bytes, info + 36, lazy.len() as u32);
    bytes
}

#[test]
fn reads_dylibs_and_bind_opcodes() {
    let bytes = macho64_with_bindings();
    let image = parse_macho(&bytes).expect("parse mach-o with bindings");
    assert_eq!(image.dylibs.len(), 1);
    assert_eq!(image.dylibs[0].name, "/usr/lib/libSystem.B.dylib");
    assert_eq!(image.dylibs[0].kind, MachDylibKind::Load);
    assert_eq!(image.dylibs[0].ordinal, Some(1));
    let info = image.dyld_info.expect("dyld info");
    assert_eq!(info.bind_offset, 0x300);
    assert_eq!(info.lazy_bind_offset, 0x320);

    let bindings = read_bindings(&bytes, &image).expect("bindings");
    assert_eq!(bindings.len(), 3);
    assert_eq!(bindings[0].symbol, "_puts");
    assert_eq!(bindings[0].library_ordinal, 1);
    assert_eq!(bindings[0].library.as_deref(), Some("/usr/lib/libSystem.B.dylib"));
    assert_eq!(bindings[0].segment, "__TEXT");
    assert_eq!(bindings[0].address, 0x1_0000_0100);
    assert!(bindings[0].weak);
    assert!(!bindings[0].lazy);
    assert_eq!(bindings[0].stream, "bind");

    assert_eq!(bindings[1].symbol, "_exit");
    assert!(bindings[1].lazy);
    assert!(!bindings[1].weak);
    assert_eq!(bindings[1].stream, "lazy_bind");
    assert_eq!(bindings[1].address, 0x1_0000_0030);
    assert_eq!(bindings[2].symbol, "_abort");
    assert!(bindings[2].lazy);
    assert_eq!(bindings[2].address, 0x1_0000_0040);
}

#[test]
fn rejects_unknown_bind_opcode() {
    let mut bytes = macho64_with_bindings();
    bytes[0x300] = 0xF0;
    let image = parse_macho(&bytes).expect("parse mach-o");
    assert!(matches!(read_bindings(&bytes, &image), Err(MachOParseError::BadBindOpcode { opcode: 0xF0, .. })));
}

#[test]
fn missing_dyld_info_yields_no_bindings() {
    let bytes = macho64_bytes();
    let image = parse_macho(&bytes).expect("parse mach-o");
    assert!(read_bindings(&bytes, &image).expect("bindings").is_empty());
}

#[test]
fn cpu_types_map_to_architectures() {
    assert_eq!(architecture(7), MachArchitecture::X86);
    assert_eq!(architecture(0x0100_0007), MachArchitecture::X64);
    assert_eq!(architecture(0x0000_000c), MachArchitecture::Arm);
    assert_eq!(architecture(0x0100_000c), MachArchitecture::Arm64);
    assert_eq!(architecture(42), MachArchitecture::Unknown);
    assert_eq!(MachArchitecture::X64.as_str(), "x64");
}

/// 在 [`macho64_bytes`] 的基础上追加 `LC_SYMTAB` 与两条符号。
fn macho64_with_symbols() -> Vec<u8> {
    let segment_size = 72 + 80;
    let command_size = segment_size + 24 + 24;
    let mut bytes = vec![0u8; 0x400];

    put_u32(&mut bytes, 0, MH_MAGIC_64);
    put_u32(&mut bytes, 4, 0x0100_000C);
    put_u32(&mut bytes, 12, 2);
    put_u32(&mut bytes, 16, 3);
    put_u32(&mut bytes, 20, command_size as u32);

    let segment = 32;
    put_u32(&mut bytes, segment, LC_SEGMENT_64);
    put_u32(&mut bytes, segment + 4, segment_size as u32);
    put_name(&mut bytes, segment + 8, b"__TEXT");
    put_u64(&mut bytes, segment + 24, 0x1_0000_0000);
    put_u64(&mut bytes, segment + 32, 0x4000);
    put_u64(&mut bytes, segment + 40, 0x200);
    put_u64(&mut bytes, segment + 48, 0x200);
    put_u32(&mut bytes, segment + 64, 1);

    let section = segment + 72;
    put_name(&mut bytes, section, b"__text");
    put_name(&mut bytes, section + 16, b"__TEXT");
    put_u64(&mut bytes, section + 32, 0x1_0000_0400);
    put_u64(&mut bytes, section + 40, 0x10);
    put_u32(&mut bytes, section + 48, 0x400);
    put_u32(&mut bytes, section + 52, 4);
    put_u32(&mut bytes, section + 64, 0x8000_0400);

    let main = segment + segment_size;
    put_u32(&mut bytes, main, LC_MAIN);
    put_u32(&mut bytes, main + 4, 24);
    put_u64(&mut bytes, main + 8, 0x400);

    let symtab = main + 24;
    put_u32(&mut bytes, symtab, LC_SYMTAB);
    put_u32(&mut bytes, symtab + 4, 24);
    put_u32(&mut bytes, symtab + 8, 0x300);
    put_u32(&mut bytes, symtab + 12, 2);
    put_u32(&mut bytes, symtab + 16, 0x320);
    put_u32(&mut bytes, symtab + 20, 14);

    let strings = b"\0_start\0_puts\0";
    bytes[0x320..0x320 + strings.len()].copy_from_slice(strings);
    put_u32(&mut bytes, 0x300, 1);
    bytes[0x304] = 0x0f;
    bytes[0x305] = 1;
    put_u64(&mut bytes, 0x308, 0x1_0000_0400);
    put_u32(&mut bytes, 0x310, 8);
    bytes[0x314] = 0x01;
    put_u16(&mut bytes, 0x316, 0);
    bytes
}

#[test]
fn reads_symtab_symbols() {
    let bytes = macho64_with_symbols();
    let image = parse_macho(&bytes).expect("parse mach-o with symtab");
    let symtab = image.symtab.expect("symtab");
    assert_eq!(symtab.symbol_count, 2);
    assert_eq!(symtab.string_offset, 0x320);

    let symbols = read_macho_symbols(&bytes, &image).expect("read symbols");
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].name, "_start");
    assert!(symbols[0].is_section());
    assert!(symbols[0].is_external());
    assert_eq!(symbols[0].value, 0x1_0000_0400);
    assert_eq!(symbols[1].name, "_puts");
    assert!(symbols[1].is_undefined());
    assert!(symbols[1].is_external());
}
