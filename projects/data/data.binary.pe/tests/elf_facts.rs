//! ELF 程序头、动态段与 note 事实解码。

use data_binary_pe::elf::{
    ElfNoteSource, ElfSegmentType, read_dynamic, read_notes, read_program_headers,
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

/// 无程序头表的 ELF64：两个节，节名表在 0x300。
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
    put_u16(&mut bytes, 58, 64);
    put_u16(&mut bytes, 60, 3);
    put_u16(&mut bytes, 62, 2);

    let names = b"\0.text\0.shstrtab\0";
    bytes[0x300..0x300 + names.len()].copy_from_slice(names);

    put_u32(&mut bytes, 0x140, 1);
    put_u32(&mut bytes, 0x144, 1);
    put_u64(&mut bytes, 0x148, 6);
    put_u64(&mut bytes, 0x150, 0x401000);
    put_u64(&mut bytes, 0x158, 0x200);
    put_u64(&mut bytes, 0x160, 0x10);
    bytes
}

/// ELF64，含 `PT_LOAD`、`PT_INTERP` 与 `PT_DYNAMIC`。
fn elf_dynamic_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x500];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 2);
    put_u16(&mut bytes, 18, 62);
    put_u32(&mut bytes, 20, 1);
    put_u64(&mut bytes, 24, 0x401000);
    put_u64(&mut bytes, 32, 0x40);
    put_u16(&mut bytes, 54, 56);
    put_u16(&mut bytes, 56, 3);

    put_u32(&mut bytes, 0x40, 1);
    put_u32(&mut bytes, 0x44, 4);
    put_u64(&mut bytes, 0x48, 0);
    put_u64(&mut bytes, 0x50, 0x400000);
    put_u64(&mut bytes, 0x60, 0x500);
    put_u64(&mut bytes, 0x68, 0x500);
    put_u64(&mut bytes, 0x70, 0x1000);

    put_u32(&mut bytes, 0x78, 3);
    put_u32(&mut bytes, 0x7c, 4);
    put_u64(&mut bytes, 0x80, 0x200);
    put_u64(&mut bytes, 0x88, 0x400200);
    put_u64(&mut bytes, 0x98, 28);
    put_u64(&mut bytes, 0xa0, 28);
    put_u64(&mut bytes, 0xa8, 1);

    put_u32(&mut bytes, 0xb0, 2);
    put_u32(&mut bytes, 0xb4, 6);
    put_u64(&mut bytes, 0xb8, 0x300);
    put_u64(&mut bytes, 0xc0, 0x400300);
    put_u64(&mut bytes, 0xd0, 128);
    put_u64(&mut bytes, 0xd8, 128);
    put_u64(&mut bytes, 0xe0, 8);

    let interpreter = b"/lib64/ld-linux-x86-64.so.2\0";
    bytes[0x200..0x200 + interpreter.len()].copy_from_slice(interpreter);

    let dynstr = b"\0libc.so.6\0libm.so.6\0libsample.so\0/opt/lib:/usr/lib\0/usr/local/lib\0";
    bytes[0x240..0x240 + dynstr.len()].copy_from_slice(dynstr);

    let entries = [(1u64, 1u64), (1, 11), (14, 21), (15, 34), (29, 52), (5, 0x400240), (10, dynstr.len() as u64), (0, 0)];
    for (index, (tag, value)) in entries.iter().enumerate() {
        let base = 0x300 + index * 16;
        put_u64(&mut bytes, base, *tag);
        put_u64(&mut bytes, base + 8, *value);
    }
    bytes
}

/// ELF64，含一个 `PT_NOTE` 段：build-id、ABI tag 与 property 各一条。
fn elf_notes_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x300];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 2);
    put_u16(&mut bytes, 18, 62);
    put_u32(&mut bytes, 20, 1);
    put_u64(&mut bytes, 24, 0x401000);
    put_u64(&mut bytes, 32, 0x40);
    put_u16(&mut bytes, 52, 64);
    put_u16(&mut bytes, 54, 56);
    put_u16(&mut bytes, 56, 1);

    put_u32(&mut bytes, 0x40, 4);
    put_u32(&mut bytes, 0x44, 4);
    put_u64(&mut bytes, 0x48, 0x200);
    put_u64(&mut bytes, 0x50, 0x400200);
    put_u64(&mut bytes, 0x60, 0x80);
    put_u64(&mut bytes, 0x68, 0x80);
    put_u64(&mut bytes, 0x70, 4);

    // GNU build id：20 字节描述。
    put_u32(&mut bytes, 0x200, 4);
    put_u32(&mut bytes, 0x204, 20);
    put_u32(&mut bytes, 0x208, 3);
    bytes[0x20c..0x210].copy_from_slice(b"GNU\0");
    for index in 0..20 {
        bytes[0x210 + index] = (index + 1) as u8;
    }

    // GNU ABI tag：os、major、minor、subminor。
    put_u32(&mut bytes, 0x224, 4);
    put_u32(&mut bytes, 0x228, 16);
    put_u32(&mut bytes, 0x22c, 1);
    bytes[0x230..0x234].copy_from_slice(b"GNU\0");
    put_u32(&mut bytes, 0x234, 0);
    put_u32(&mut bytes, 0x238, 6);
    put_u32(&mut bytes, 0x23c, 8);
    put_u32(&mut bytes, 0x240, 0);

    // GNU property：一条 X86_FEATURE_1_AND。
    put_u32(&mut bytes, 0x244, 4);
    put_u32(&mut bytes, 0x248, 16);
    put_u32(&mut bytes, 0x24c, 5);
    bytes[0x250..0x254].copy_from_slice(b"GNU\0");
    put_u32(&mut bytes, 0x254, 0xc000_0002);
    put_u32(&mut bytes, 0x258, 4);
    put_u32(&mut bytes, 0x25c, 3);
    bytes
}

#[test]
fn elf_program_headers_decode_the_table() {
    let report = read_program_headers(&elf_notes_bytes()).expect("ELF program headers");

    assert_eq!(report.total, 1);
    assert!(!report.truncated);
    assert_eq!(report.segments.len(), 1);
    let segment = &report.segments[0];
    assert!(report.is_64);
    assert_eq!(segment.index, 0);
    assert_eq!(segment.segment_type, ElfSegmentType::Note);
    assert_eq!(segment.segment_type.name(), "PT_NOTE");
    assert_eq!(segment.segment_type.value(), 4);
    assert_eq!(segment.offset, 0x200);
    assert_eq!(segment.virtual_address, 0x400200);
    assert_eq!(segment.file_size, 0x80);
    assert_eq!(segment.memory_size, 0x80);
    assert_eq!(segment.flags, 4);
    assert_eq!(segment.flag_text(), "R");
    assert!(segment.readable() && !segment.writable() && !segment.executable());
    assert_eq!(segment.alignment, 4);
}

#[test]
fn elf_program_headers_report_absence() {
    let report = read_program_headers(&elf64_bytes()).expect("ELF program headers");
    assert_eq!(report.total, 0);
    assert!(report.segments.is_empty());
    assert!(!report.truncated);
}

#[test]
fn elf_program_headers_reject_non_elf() {
    let mut bytes = vec![0u8; 64];
    bytes[0..2].copy_from_slice(b"MZ");
    let error = read_program_headers(&bytes).expect_err("non-ELF");
    assert_eq!(error.code(), "elf/bad-magic");
}

#[test]
fn elf_dynamic_reports_interpreter_needed_and_search_paths() {
    let report = read_dynamic(&elf_dynamic_bytes()).expect("ELF dynamic report");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.interpreter.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
    assert_eq!(report.soname.as_deref(), Some("libsample.so"));
    assert_eq!(report.needed, ["libc.so.6", "libm.so.6"]);
    assert_eq!(report.rpath, ["/opt/lib", "/usr/lib"]);
    assert_eq!(report.runpath, ["/usr/local/lib"]);
    assert_eq!(report.dynamic_entries, 7);
}

#[test]
fn elf_dynamic_reports_static_images() {
    let report = read_dynamic(&elf64_bytes()).expect("ELF dynamic report");
    assert!(!report.present);
    assert_eq!(report.interpreter, None);
    assert!(report.needed.is_empty());
    assert!(report.rpath.is_empty());
    assert_eq!(report.dynamic_entries, 0);
}

#[test]
fn elf_notes_decode_build_id_abi_tag_and_properties() {
    let report = read_notes(&elf_notes_bytes()).expect("ELF notes");

    assert!(report.present);
    assert!(!report.truncated);
    assert_eq!(report.source, Some(ElfNoteSource::Segments));
    assert_eq!(report.source.map(ElfNoteSource::as_str), Some("segments"));
    assert_eq!(report.total, 3);
    assert_eq!(report.notes.len(), 3);
    assert_eq!(report.build_id.as_deref(), Some("0102030405060708090a0b0c0d0e0f1011121314"));

    let abi = report.abi_tag.as_ref().expect("GNU ABI tag");
    assert_eq!(abi.os, "Linux");
    assert_eq!((abi.major, abi.minor, abi.subminor), (6, 8, 0));

    assert_eq!(report.notes[0].owner, "GNU");
    assert_eq!(report.notes[0].note_type, "NT_GNU_BUILD_ID");
    assert_eq!(report.notes[1].note_type, "NT_GNU_ABI_TAG");
    assert_eq!(report.notes[1].description, "Linux 6.8.0");
    assert_eq!(report.notes[2].note_type, "NT_GNU_PROPERTY_TYPE_0");

    assert_eq!(report.properties.len(), 1);
    assert_eq!(report.properties[0].property_type, "X86_FEATURE_1_AND");
    assert_eq!(report.properties[0].data_size, 4);
    assert_eq!(report.properties[0].data, "03000000");
}

#[test]
fn elf_notes_report_absence() {
    let report = read_notes(&elf64_bytes()).expect("ELF notes");
    assert!(!report.present);
    assert_eq!(report.source, None);
    assert_eq!(report.build_id, None);
    assert!(report.notes.is_empty());
    assert_eq!(report.total, 0);
}
