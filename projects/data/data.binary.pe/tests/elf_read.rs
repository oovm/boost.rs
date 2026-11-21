use data_binary_pe::elf::{
    ElfArchitecture, ElfClass, ElfEndian, ElfParseError, ElfSection, ElfSectionLayout, NativeElfImage, NativeElfWriter, address_to_offset,
    architecture, parse_elf, parse_elf64, read_elf_symbols, sections_of_type,
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

const NAMES: &[u8] = b"\0.text\0.shstrtab\0";

/// ELF64 小端：3 个节，节名表在 0x300。
fn elf64_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x400];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 2);
    put_u16(&mut bytes, 18, 62);
    put_u32(&mut bytes, 20, 1);
    put_u64(&mut bytes, 24, 0x401000);
    put_u64(&mut bytes, 40, 0x100);
    put_u16(&mut bytes, 52, 64);
    put_u16(&mut bytes, 58, 64);
    put_u16(&mut bytes, 60, 3);
    put_u16(&mut bytes, 62, 2);
    bytes[0x300..0x300 + NAMES.len()].copy_from_slice(NAMES);

    put_u32(&mut bytes, 0x140, 1);
    put_u32(&mut bytes, 0x144, 1);
    put_u64(&mut bytes, 0x148, 6);
    put_u64(&mut bytes, 0x150, 0x401000);
    put_u64(&mut bytes, 0x158, 0x200);
    put_u64(&mut bytes, 0x160, 0x10);
    put_u64(&mut bytes, 0x170, 16);

    put_u32(&mut bytes, 0x180, 7);
    put_u32(&mut bytes, 0x184, 3);
    put_u64(&mut bytes, 0x198, 0x300);
    put_u64(&mut bytes, 0x1a0, NAMES.len() as u64);
    bytes
}

#[test]
fn parses_native_elf_writer_output() {
    let image = NativeElfImage { text: vec![0x31, 0xC0, 0xC3], rodata: Vec::new(), entry_point: 0 };
    let bytes = NativeElfWriter::write_executable(&image).expect("write elf");
    let header = parse_elf64(&bytes).expect("parse elf");
    assert_eq!(header.file_type, 2);
    assert_eq!(header.machine, 62);

    let layout = ElfSectionLayout::compute(image.text.len() as u32, 0);
    let parsed = parse_elf(&bytes).expect("parse full elf");
    assert_eq!(parsed.header.class, ElfClass::Elf64);
    assert_eq!(parsed.header.endian, ElfEndian::Little);
    assert_eq!(parsed.header.entry, 0x400000 + u64::from(layout.text_offset) + u64::from(image.entry_point));
    assert!(parsed.sections.is_empty());
}

#[test]
fn reads_elf64_section_table_with_names() {
    let image = parse_elf(&elf64_bytes()).expect("parse elf64");

    assert_eq!(image.header.class, ElfClass::Elf64);
    assert_eq!(image.header.endian, ElfEndian::Little);
    assert_eq!(image.header.file_type, 2);
    assert_eq!(image.header.machine, 62);
    assert_eq!(image.header.entry, 0x401000);
    assert_eq!(image.header.section_count, 3);
    assert_eq!(image.header.section_name_index, 2);

    assert_eq!(image.sections.len(), 3);
    assert_eq!(image.sections[0].name, "");
    assert_eq!(image.sections[1].name, ".text");
    assert_eq!(image.sections[1].section_type, 1);
    assert_eq!(image.sections[1].flags, 6);
    assert_eq!(image.sections[1].address, 0x401000);
    assert_eq!(image.sections[1].offset, 0x200);
    assert_eq!(image.sections[1].size, 0x10);
    assert_eq!(image.sections[1].alignment, 16);
    assert_eq!(image.sections[2].name, ".shstrtab");
    assert_eq!(image.sections[2].section_type, 3);
}

#[test]
fn reads_elf32_section_table() {
    let mut bytes = vec![0u8; 0x300];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 1;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 2);
    put_u16(&mut bytes, 18, 3);
    put_u32(&mut bytes, 20, 1);
    put_u32(&mut bytes, 24, 0x8048000);
    put_u32(&mut bytes, 32, 0x80);
    put_u16(&mut bytes, 40, 52);
    put_u16(&mut bytes, 46, 40);
    put_u16(&mut bytes, 48, 3);
    put_u16(&mut bytes, 50, 2);
    bytes[0x100..0x100 + NAMES.len()].copy_from_slice(NAMES);

    put_u32(&mut bytes, 0xa8, 1);
    put_u32(&mut bytes, 0xac, 1);
    put_u32(&mut bytes, 0xb0, 6);
    put_u32(&mut bytes, 0xb4, 0x8048000);
    put_u32(&mut bytes, 0xb8, 0x200);
    put_u32(&mut bytes, 0xbc, 0x10);

    put_u32(&mut bytes, 0xd0, 7);
    put_u32(&mut bytes, 0xd4, 3);
    put_u32(&mut bytes, 0xe0, 0x100);
    put_u32(&mut bytes, 0xe4, NAMES.len() as u32);

    let image = parse_elf(&bytes).expect("parse elf32");
    assert_eq!(image.header.class, ElfClass::Elf32);
    assert_eq!(image.header.machine, 3);
    assert_eq!(image.header.entry, 0x8048000);
    assert_eq!(image.sections.len(), 3);
    assert_eq!(image.sections[1].name, ".text");
    assert_eq!(image.sections[1].flags, 6);
    assert_eq!(image.sections[1].address, 0x8048000);
    assert_eq!(image.sections[1].offset, 0x200);
    assert_eq!(image.sections[2].name, ".shstrtab");
}

#[test]
fn reads_big_endian_elf_header() {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 2;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&2u16.to_be_bytes());
    bytes[18..20].copy_from_slice(&20u16.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..32].copy_from_slice(&0x1000u64.to_be_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_be_bytes());

    let image = parse_elf(&bytes).expect("parse big-endian elf");
    assert_eq!(image.header.endian, ElfEndian::Big);
    assert_eq!(image.header.machine, 20);
    assert_eq!(image.header.entry, 0x1000);
    assert!(image.sections.is_empty());
}

#[test]
fn expands_extended_section_numbering() {
    let mut bytes = elf64_bytes();
    put_u16(&mut bytes, 60, 0);
    put_u16(&mut bytes, 62, 0xffff);
    put_u64(&mut bytes, 0x120, 3);
    put_u32(&mut bytes, 0x128, 2);

    let image = parse_elf(&bytes).expect("parse extended numbering");
    assert_eq!(image.header.section_count, 3);
    assert_eq!(image.header.section_name_index, 2);
    assert_eq!(image.sections[1].name, ".text");
}

#[test]
fn rejects_truncated_and_malformed_tables() {
    let mut bytes = elf64_bytes();
    bytes.truncate(0x120);
    assert!(matches!(parse_elf(&bytes), Err(ElfParseError::SectionTableOutOfRange { .. })));

    let mut bytes = elf64_bytes();
    put_u16(&mut bytes, 58, 32);
    assert!(matches!(parse_elf(&bytes), Err(ElfParseError::SectionEntryTooSmall { .. })));

    let mut bytes = elf64_bytes();
    put_u16(&mut bytes, 62, 7);
    assert!(matches!(parse_elf(&bytes), Err(ElfParseError::SectionNameTableOutOfRange { .. })));

    let mut not_elf = vec![0u8; 64];
    not_elf[..4].copy_from_slice(b"NOPE");
    assert!(matches!(parse_elf(&not_elf), Err(ElfParseError::BadMagic)));

    let mut bad_class = vec![0u8; 64];
    bad_class[..4].copy_from_slice(b"\x7fELF");
    bad_class[4] = 9;
    assert!(matches!(parse_elf(&bad_class), Err(ElfParseError::UnsupportedClass(9))));

    let mut bad_encoding = vec![0u8; 64];
    bad_encoding[..4].copy_from_slice(b"\x7fELF");
    bad_encoding[4] = 2;
    bad_encoding[5] = 7;
    assert!(matches!(parse_elf(&bad_encoding), Err(ElfParseError::UnsupportedEncoding(7))));

    assert!(matches!(parse_elf(b"\x7fELF"), Err(ElfParseError::TooShort)));
}

fn section(name: &str, address: u64, offset: u64, size: u64) -> ElfSection {
    ElfSection {
        name: name.to_string(),
        section_type: 1,
        flags: 0,
        address,
        offset,
        size,
        link: 0,
        info: 0,
        alignment: 16,
        entry_size: 0,
    }
}

#[test]
fn machine_values_map_to_architectures() {
    assert_eq!(architecture(3), ElfArchitecture::X86);
    assert_eq!(architecture(62), ElfArchitecture::X64);
    assert_eq!(architecture(40), ElfArchitecture::Arm);
    assert_eq!(architecture(183), ElfArchitecture::Arm64);
    assert_eq!(architecture(999), ElfArchitecture::Unknown);
    assert_eq!(ElfArchitecture::Arm64.as_str(), "arm64");
}

#[test]
fn addresses_map_through_non_empty_sections() {
    let sections = vec![
        section(".text", 0x1000, 0x0, 0x200),
        section(".empty", 0x2000, 0x200, 0),
        section(".data", 0x4000, 0x400, 0x100),
    ];
    assert_eq!(address_to_offset(&sections, 0x1010), Some(0x10));
    assert_eq!(address_to_offset(&sections, 0x4000), Some(0x400));
    assert_eq!(address_to_offset(&sections, 0x40ff), Some(0x4ff));
    assert_eq!(address_to_offset(&sections, 0x2000), None);
    assert_eq!(address_to_offset(&sections, 0x5000), None);
}

/// 五个节的 ELF64：空节、`.text`、`.dynstr`、`.dynsym`、`.shstrtab`。
fn elf64_with_symbols() -> Vec<u8> {
    const SH_NAMES: &[u8] = b"\0.text\0.dynstr\0.dynsym\0.shstrtab\0";
    const DYN_STRINGS: &[u8] = b"\0puts\0my_export\0";
    let mut bytes = vec![0u8; 0x400];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 3);
    put_u16(&mut bytes, 18, 62);
    put_u32(&mut bytes, 20, 1);
    put_u64(&mut bytes, 24, 0x401000);
    put_u64(&mut bytes, 40, 0x200);
    put_u16(&mut bytes, 52, 64);
    put_u16(&mut bytes, 58, 64);
    put_u16(&mut bytes, 60, 5);
    put_u16(&mut bytes, 62, 4);

    bytes[0x100..0x100 + SH_NAMES.len()].copy_from_slice(SH_NAMES);
    bytes[0x140..0x140 + DYN_STRINGS.len()].copy_from_slice(DYN_STRINGS);

    // `.dynsym` 三项：空项、未定义的 `puts`、已定义的 `my_export`。
    put_u32(&mut bytes, 0x180 + 24, 1);
    bytes[0x180 + 28] = 0x12;
    put_u32(&mut bytes, 0x180 + 48, 6);
    bytes[0x180 + 52] = 0x12;
    put_u16(&mut bytes, 0x180 + 54, 1);
    put_u64(&mut bytes, 0x180 + 56, 0x401000);
    put_u64(&mut bytes, 0x180 + 64, 0x10);

    // 节 1：`.text`。
    put_u32(&mut bytes, 0x200 + 64, 1);
    put_u32(&mut bytes, 0x200 + 64 + 4, 1);
    put_u64(&mut bytes, 0x200 + 64 + 8, 6);
    put_u64(&mut bytes, 0x200 + 64 + 16, 0x401000);
    put_u64(&mut bytes, 0x200 + 64 + 24, 0);
    put_u64(&mut bytes, 0x200 + 64 + 32, 0x10);
    // 节 2：`.dynstr`。
    put_u32(&mut bytes, 0x200 + 128, 7);
    put_u32(&mut bytes, 0x200 + 128 + 4, 3);
    put_u64(&mut bytes, 0x200 + 128 + 8, 2);
    put_u64(&mut bytes, 0x200 + 128 + 16, 0x402140);
    put_u64(&mut bytes, 0x200 + 128 + 24, 0x140);
    put_u64(&mut bytes, 0x200 + 128 + 32, DYN_STRINGS.len() as u64);
    // 节 3：`.dynsym`。
    put_u32(&mut bytes, 0x200 + 192, 15);
    put_u32(&mut bytes, 0x200 + 192 + 4, 11);
    put_u64(&mut bytes, 0x200 + 192 + 8, 2);
    put_u64(&mut bytes, 0x200 + 192 + 16, 0x402180);
    put_u64(&mut bytes, 0x200 + 192 + 24, 0x180);
    put_u64(&mut bytes, 0x200 + 192 + 32, 72);
    put_u32(&mut bytes, 0x200 + 192 + 40, 2);
    put_u64(&mut bytes, 0x200 + 192 + 48, 8);
    put_u64(&mut bytes, 0x200 + 192 + 56, 24);
    // 节 4：`.shstrtab`。
    put_u32(&mut bytes, 0x200 + 256, 22);
    put_u32(&mut bytes, 0x200 + 256 + 4, 3);
    put_u64(&mut bytes, 0x200 + 256 + 16, 0x100);
    put_u64(&mut bytes, 0x200 + 256 + 24, 0x100);
    put_u64(&mut bytes, 0x200 + 256 + 32, SH_NAMES.len() as u64);
    bytes
}

#[test]
fn reads_dynamic_symbols_with_names() {
    let bytes = elf64_with_symbols();
    let image = parse_elf(&bytes).expect("parse elf64 with symbols");
    let tables = sections_of_type(&image, 11);
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, ".dynsym");

    let symbols = read_elf_symbols(&bytes, &image, tables[0]).expect("read symbols");
    assert_eq!(symbols.len(), 3);
    assert_eq!(symbols[1].name, "puts");
    assert!(symbols[1].is_undefined());
    assert!(symbols[1].is_global());
    assert_eq!(symbols[1].kind(), 2);
    assert_eq!(symbols[2].name, "my_export");
    assert!(!symbols[2].is_undefined());
    assert_eq!(symbols[2].value, 0x401000);
    assert_eq!(symbols[2].size, 0x10);
}
