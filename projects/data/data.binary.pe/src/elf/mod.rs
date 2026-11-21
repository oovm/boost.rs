//! 最小 `ELF64` 可执行镜像（`ET_EXEC`，x86-64）与 `ET_DYN` 共享库（AArch64）。

mod layout;
mod read;
mod shared_writer;
mod writer;

pub use layout::ElfSectionLayout;
pub use read::{
    Elf64Header, Elf64ParseError, ElfArchitecture, ElfClass, ElfEndian, ElfHeader, ElfImage, ElfParseError, ElfSection, ElfSymbol,
    architecture, address_to_offset, parse_elf, parse_elf64, read_elf_symbols, sections_of_type,
};
pub use shared_writer::{
    SharedElfDataReference, SharedElfDataRelocation, SharedElfDataTarget, SharedElfImage, SharedElfImportCall, SharedElfSpec,
    SharedElfWriteError, SharedElfWriter, SharedObjectExport,
};
pub use writer::{ElfWriteError, NativeElfImage, NativeElfWriter};
