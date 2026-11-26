//! `PE/COFF` 解析与原生 / 托管写出。

mod analysis;
mod bound_imports;
mod certificates;
pub mod clr;
mod delay_imports;
mod exception;
mod load_config;
mod mitigations;
mod native;
mod read;
mod relocations;
mod resources;
mod rich_header;
mod tls;
mod version;
mod writer;

pub use analysis::{
    PeAnalysisError, PeCodeView, PeDebugEntry, PeExport, PeImport, PeSymbol, read_coff_symbols, read_debug_directory, read_exports,
    read_imports,
};
pub use bound_imports::{
    PeBoundForwarder, PeBoundImportsReport, PeBoundModule, read_bound_imports,
};
pub use certificates::{PeCertificate, PeCertificatesReport, read_certificates};
pub use delay_imports::{PeDelayImportsReport, PeDelayModule, PeDelayThunk, read_delay_imports};
pub use exception::{PeExceptionReport, PeRuntimeFunction, PeUnwindInfo, read_exception_directory};
pub use load_config::{PeCodeIntegrityInfo, PeLoadConfigReport, read_load_config};
pub use mitigations::{PeMitigationsReport, PeWritableExecutableSection, read_mitigations};
pub use relocations::{PeRelocationBlock, PeRelocationKindCount, PeRelocationsReport, read_relocations};
pub use resources::{PeResource, PeResourcesReport, read_resources};
pub use rich_header::{PeRichEntry, PeRichHeaderReport, read_rich_header};
pub use tls::{PeTlsCallback, PeTlsReport, read_tls_directory};
pub use version::{PeFixedFileInfo, PeVersionReport, PeVersionString, read_version_resources};
pub use clr::{
    ClrImageKind, ClrMetadataBuilder, ClrMetadataError, PeWriter, PeWriterError, PeWriterOptions, UserStringsHeap,
    method_body::{FatFormat, IlInstruction, LocalVarFlags, LocalVarType, MethodBody, MethodBodyFlags, parse_method_body_il},
    table_sizes::{CodedIndex, coded_index_size, heap_index_size, table_data_offset, table_index_size, table_row_size},
    tables::{
        ElementType, MemberRefParent, MethodSignature, decode_element_type, decode_member_ref_parent, decode_method_signature, read_idx,
        read_u32_table,
    },
};
pub use native::{
    IMAGE_BASE_X64, NativeDataRelocation, NativeDataSection, NativeDataTarget, NativeDllImport, NativeExport, NativeImportTable, NativePeImage,
    NativePeWriter, NativeSectionLayout, build_native_import_table,
};
pub use read::{
    CliHeader, CoffHeader as PeCoffHeader, DataDirectory, DosHeader, MetadataRoot, OptionalHeader, Pe64Header, Pe64ParseError, PeImage,
    PeParseError, SectionHeader, StreamHeader, TableKind, extract_pe_section, parse_pe, parse_pe64, read_blob, read_compressed_uint,
    read_strings_string, read_user_string, rva_to_offset,
};
