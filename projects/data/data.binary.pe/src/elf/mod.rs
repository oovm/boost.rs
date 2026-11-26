//! 最小 `ELF64` 可执行镜像（`ET_EXEC`，x86-64）与 `ET_DYN` 共享库（AArch64）。

mod dynamic;
mod layout;
mod notes;
mod read;
mod segments;
mod shared_writer;
mod writer;

pub use dynamic::{ElfDynamic, read_dynamic};
pub use layout::ElfSectionLayout;
pub use notes::{ElfAbiTag, ElfGnuProperty, ElfNote, ElfNoteSource, ElfNotes, read_notes};
pub use read::{
    Elf64Header, Elf64ParseError, ElfArchitecture, ElfClass, ElfEndian, ElfHeader, ElfImage, ElfParseError, ElfSection, ElfSymbol,
    architecture, address_to_offset, parse_elf, parse_elf64, parse_elf_header, read_elf_symbols, sections_of_type,
};
pub use segments::{ElfProgramHeaders, ElfSegment, ElfSegmentType, read_program_headers};
pub use shared_writer::{
    SharedElfDataReference, SharedElfDataRelocation, SharedElfDataTarget, SharedElfImage, SharedElfImportCall, SharedElfSpec,
    SharedElfWriteError, SharedElfWriter, SharedObjectExport,
};
pub use writer::{ElfWriteError, NativeElfImage, NativeElfWriter};
