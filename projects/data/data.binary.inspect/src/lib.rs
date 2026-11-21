//! File-backed binary inspection sessions.
//!
//! This crate is the shared engine behind the OpenX binary tools. It owns the
//! operations every tool repeats — open, header summary, section table, string
//! scan, hex dump, byte reads, RVA mapping and atomic patching — while the
//! format decoders stay in `data-binary-pe` and `data-binary-mach-o`.
//!
//! Product policy (which defaults to use, which formats to expose, how to name
//! errors) stays in the product crate; this crate only performs the operation.

#![warn(missing_docs)]

use std::{
    fmt,
    fs,
    path::{Path, PathBuf},
};

use data_binary_mach_o::mach_o;
use data_binary_pe::{elf, pe};
use boost_fs::publish::{OverwritePolicy, PublishOptions};
use data_checksum::XxHash64;
use data_encoding::representation::Hex;
use serde::{Deserialize, Serialize};

/// Failure classes shared by every inspection operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    /// The file could not be read or written.
    Io(String),
    /// The bytes did not decode as the reported container.
    Parse(String),
    /// The operation is outside this crate's declared surface.
    Unsupported(String),
    /// A byte range or address is invalid for this file.
    Range(String),
}

impl InspectError {
    /// Stable wire code, matching the OpenX error contract.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "binary/io",
            Self::Parse(_) => "binary/parse",
            Self::Unsupported(_) => "binary/unsupported",
            Self::Range(_) => "binary/range",
        }
    }

    /// Human-readable message without the code prefix.
    pub fn message(&self) -> &str {
        match self {
            Self::Io(message) | Self::Parse(message) | Self::Unsupported(message) | Self::Range(message) => message,
        }
    }

    /// Builds an I/O failure.
    pub fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }

    /// Builds a parse failure.
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse(message.into())
    }

    /// Builds an unsupported-operation failure.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    /// Builds a range failure.
    pub fn range(message: impl Into<String>) -> Self {
        Self::Range(message.into())
    }
}

impl fmt::Display for InspectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for InspectError {}

/// Result alias for inspection operations.
pub type Result<T> = std::result::Result<T, InspectError>;

/// Containers this crate can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryFormat {
    /// Portable Executable.
    Pe,
    /// Executable and Linkable Format.
    Elf,
    /// Mach-O.
    MachO,
    /// Unrecognised bytes.
    Unknown,
}

impl BinaryFormat {
    /// Stable lower-case name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pe => "pe",
            Self::Elf => "elf",
            Self::MachO => "macho",
            Self::Unknown => "unknown",
        }
    }
}

/// Instruction-set family inferred from the container header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    /// 32-bit x86.
    X86,
    /// 64-bit x86.
    X64,
    /// 64-bit ARM.
    Arm64,
    /// Not reported by the header.
    Unknown,
}

impl Architecture {
    /// Stable lower-case name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
            Self::Unknown => "unknown",
        }
    }
}

/// One section table entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionInfo {
    /// Section name.
    pub name: String,
    /// Virtual address of the first byte.
    pub virtual_address: u64,
    /// Size in memory.
    pub virtual_size: u64,
    /// File offset of the first byte.
    pub raw_pointer: u64,
    /// Size in the file.
    pub raw_size: u64,
    /// Format-specific flags.
    pub characteristics: u64,
    /// Whether the section is readable.
    pub readable: bool,
    /// Whether the section is writable.
    pub writable: bool,
    /// Whether the section is executable.
    pub executable: bool,
}

/// One printable ASCII run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringHit {
    /// File offset of the first byte.
    pub offset: u64,
    /// Number of bytes in the run.
    pub length: u64,
    /// The run as UTF-8 text.
    pub value: String,
}

/// Bounded result of [`BinarySession::strings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringsReport {
    /// Hits that survived the limit.
    pub hits: Vec<StringHit>,
    /// Number of hits before the limit was applied.
    pub total: usize,
    /// Whether the limit removed hits.
    pub truncated: bool,
}

/// Header summary returned for any open file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryInfo {
    /// Caller-supplied path.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// Detected container.
    pub format: String,
    /// Detected architecture.
    pub architecture: String,
    /// XXH64 hash of the whole file, as 16 hex digits.
    pub hash: String,
    /// Container header flavour.
    pub magic: Option<String>,
    /// Byte order reported by the container.
    pub endian: Option<String>,
    /// Absolute entry point, when reported.
    pub entry: Option<u64>,
    /// Entry point relative to the image base (PE only).
    pub entry_rva: Option<u64>,
    /// Image base (PE only).
    pub image_base: Option<u64>,
    /// Number of section table entries.
    pub section_count: u64,
    /// Mach-O CPU type.
    pub cpu_type: Option<i64>,
    /// Mach-O file type.
    pub file_type: Option<u32>,
    /// Mach-O load-command count.
    pub command_count: Option<u32>,
}

/// Result of one atomic patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchReport {
    /// File that received the patched bytes.
    pub path: String,
    /// Offset that was changed.
    pub offset: u64,
    /// Number of bytes written.
    pub length: u64,
    /// XXH64 hash before patching.
    pub before_hash: String,
    /// XXH64 hash after patching.
    pub after_hash: String,
}

fn default_min_string_len() -> usize {
    4
}

fn default_strings_limit() -> Option<usize> {
    Some(100)
}

fn default_hexdump_length() -> u64 {
    256
}

/// Inputs of one inspection session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectOptions {
    /// Binary to open.
    pub path: PathBuf,
    /// Minimum printable run length used when no override is given.
    #[serde(default = "default_min_string_len")]
    pub min_string_len: usize,
    /// Default hit limit; `null` means unlimited.
    #[serde(default = "default_strings_limit")]
    pub strings_limit: Option<usize>,
    /// Default byte count used by hex dumps.
    #[serde(default = "default_hexdump_length")]
    pub hexdump_length: u64,
}

impl InspectOptions {
    /// Builds options with the documented defaults for one path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            min_string_len: default_min_string_len(),
            strings_limit: default_strings_limit(),
            hexdump_length: default_hexdump_length(),
        }
    }
}

/// One opened file with its bytes, container and defaults.
#[derive(Debug, Clone)]
pub struct BinarySession {
    path: PathBuf,
    bytes: Vec<u8>,
    format: BinaryFormat,
    options: InspectOptions,
}

impl BinarySession {
    /// Opens the file named by `options`.
    pub fn open(options: InspectOptions) -> Result<Self> {
        let bytes = fs::read(&options.path)
            .map_err(|error| InspectError::io(format!("cannot read {}: {error}", options.path.display())))?;
        Ok(Self::with_options(options.path.clone(), bytes, options))
    }

    /// Builds a session over bytes already in memory.
    pub fn from_bytes(path: impl Into<PathBuf>, bytes: Vec<u8>) -> Self {
        let path = path.into();
        let options = InspectOptions::new(path.clone());
        Self::with_options(path, bytes, options)
    }

    /// Builds a session over memory with explicit defaults.
    pub fn with_options(path: PathBuf, bytes: Vec<u8>, options: InspectOptions) -> Self {
        let format = detect(&bytes);
        Self { path, bytes, format, options }
    }

    /// Builds a session from its JSON wire form.
    pub fn from_options_json(raw: &str) -> Result<Self> {
        let options: InspectOptions = serde_json::from_str(raw)
            .map_err(|error| InspectError::parse(format!("invalid InspectOptions: {error}")))?;
        Self::open(options)
    }

    /// Options this session was built from.
    pub fn options(&self) -> &InspectOptions {
        &self.options
    }

    /// Path of the open file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Detected container.
    pub fn format(&self) -> BinaryFormat {
        self.format
    }

    /// File size in bytes.
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Header summary.
    pub fn info(&self) -> Result<BinaryInfo> {
        let mut info = BinaryInfo {
            path: self.path.display().to_string(),
            size: self.size(),
            format: self.format.as_str().to_string(),
            architecture: self.architecture().as_str().to_string(),
            hash: content_hash(&self.bytes),
            magic: None,
            endian: None,
            entry: None,
            entry_rva: None,
            image_base: None,
            section_count: 0,
            cpu_type: None,
            file_type: None,
            command_count: None,
        };
        match self.format {
            BinaryFormat::Pe => {
                let image = pe::parse_pe(&self.bytes).map_err(|error| InspectError::parse(format!("PE: {error}")))?;
                info.magic = Some(pe_magic(image.optional.magic).to_string());
                info.endian = Some("little".to_string());
                info.entry = Some(image.optional.image_base.saturating_add(u64::from(image.optional.address_of_entry_point)));
                info.entry_rva = Some(u64::from(image.optional.address_of_entry_point));
                info.image_base = Some(image.optional.image_base);
                info.section_count = image.sections.len() as u64;
            }
            BinaryFormat::Elf => {
                let image = elf::parse_elf(&self.bytes).map_err(|error| InspectError::parse(format!("ELF: {error}")))?;
                info.magic = Some(image.header.class.to_string());
                info.endian = Some(match image.header.endian {
                    elf::ElfEndian::Little => "little",
                    elf::ElfEndian::Big => "big",
                }
                .to_string());
                info.entry = Some(image.header.entry);
                info.section_count = image.sections.len() as u64;
            }
            BinaryFormat::MachO => {
                let header = mach_o::parse_macho_header(&self.bytes)
                    .map_err(|error| InspectError::parse(format!("Mach-O: {error}")))?;
                let is_64 = mach_o::macho_magic(&self.bytes).map(|(is_64, _)| is_64).unwrap_or(false);
                info.magic = Some(if is_64 { "Mach-O 64" } else { "Mach-O 32" }.to_string());
                info.endian = Some(match header.endian {
                    mach_o::MachOEndian::Little => "little",
                    mach_o::MachOEndian::Big => "big",
                }
                .to_string());
                info.cpu_type = Some(i64::from(header.cpu_type));
                info.file_type = Some(header.file_type);
                info.command_count = Some(header.command_count);
            }
            BinaryFormat::Unknown => {}
        }
        Ok(info)
    }

    /// Detected architecture, or `unknown`.
    pub fn architecture(&self) -> Architecture {
        match self.format {
            BinaryFormat::Pe => pe::parse_pe(&self.bytes)
                .map(|image| pe_architecture(image.coff.machine))
                .unwrap_or(Architecture::Unknown),
            BinaryFormat::Elf => elf::parse_elf(&self.bytes)
                .map(|image| match elf::architecture(image.header.machine) {
                    elf::ElfArchitecture::X86 => Architecture::X86,
                    elf::ElfArchitecture::X64 => Architecture::X64,
                    elf::ElfArchitecture::Arm64 => Architecture::Arm64,
                    elf::ElfArchitecture::Unknown => Architecture::Unknown,
                })
                .unwrap_or(Architecture::Unknown),
            BinaryFormat::MachO => mach_o::parse_macho_header(&self.bytes)
                .map(|header| match mach_o::architecture(header.cpu_type) {
                    mach_o::MachArchitecture::X86 => Architecture::X86,
                    mach_o::MachArchitecture::X64 => Architecture::X64,
                    mach_o::MachArchitecture::Arm64 => Architecture::Arm64,
                    mach_o::MachArchitecture::Unknown => Architecture::Unknown,
                })
                .unwrap_or(Architecture::Unknown),
            BinaryFormat::Unknown => Architecture::Unknown,
        }
    }

    /// Decodes the section table.
    pub fn sections(&self) -> Result<Vec<SectionInfo>> {
        match self.format {
            BinaryFormat::Pe => {
                let image = pe::parse_pe(&self.bytes).map_err(|error| InspectError::parse(format!("PE: {error}")))?;
                Ok(image.sections.iter().map(|section| pe_section(section)).collect())
            }
            BinaryFormat::Elf => {
                let image = elf::parse_elf(&self.bytes).map_err(|error| InspectError::parse(format!("ELF: {error}")))?;
                Ok(image.sections.iter().map(|section| elf_section(section)).collect())
            }
            BinaryFormat::MachO => {
                let image = mach_o::parse_macho(&self.bytes).map_err(|error| InspectError::parse(format!("Mach-O: {error}")))?;
                Ok(image.sections.iter().map(|section| macho_section(section)).collect())
            }
            BinaryFormat::Unknown => Err(InspectError::unsupported("sections are only available for PE, ELF and Mach-O files")),
        }
    }

    /// Extracts printable ASCII runs, falling back to the session defaults.
    pub fn strings(&self, min_len: Option<usize>, limit: Option<usize>) -> StringsReport {
        let mut hits = scan_strings(&self.bytes, min_len.unwrap_or(self.options.min_string_len));
        let total = hits.len();
        let limit = limit.or(self.options.strings_limit).unwrap_or(usize::MAX);
        let truncated = total > limit;
        hits.truncate(limit);
        StringsReport { hits, total, truncated }
    }

    /// Renders a hex dump, falling back to the session defaults.
    pub fn hexdump(&self, offset: Option<u64>, length: Option<u64>) -> Result<String> {
        let offset = offset.unwrap_or(0);
        let length = length.unwrap_or(self.options.hexdump_length);
        let data = self.read(offset, length)?;
        let mut output = String::new();
        for (row, chunk) in data.chunks(16).enumerate() {
            let row_offset = offset + row as u64 * 16;
            output.push_str(&format!("{row_offset:08x}  "));
            for index in 0..16 {
                if index == 8 {
                    output.push(' ');
                }
                match chunk.get(index) {
                    Some(byte) => output.push_str(&format!("{byte:02x} ")),
                    None => output.push_str("   "),
                }
            }
            output.push_str(" |");
            for byte in chunk {
                output.push(if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' });
            }
            output.push_str("|\n");
        }
        Ok(output)
    }

    /// Reads a byte range from the file.
    pub fn read(&self, offset: u64, length: u64) -> Result<&[u8]> {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| InspectError::range(format!("read at 0x{offset:x} overflows")))?;
        if end > self.bytes.len() as u64 {
            return Err(InspectError::range(format!(
                "read of {length} byte(s) at 0x{offset:x} is outside the file (size {})",
                self.bytes.len()
            )));
        }
        Ok(&self.bytes[offset as usize..end as usize])
    }

    /// Writes patched bytes back to the source, or to `output` when given.
    pub fn patch(&mut self, offset: u64, data: &[u8], output: Option<&Path>) -> Result<PatchReport> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| InspectError::range(format!("patch at 0x{offset:x} overflows")))?;
        if end > self.bytes.len() as u64 {
            return Err(InspectError::range(format!(
                "patch of {} byte(s) at 0x{offset:x} is outside the file (size {})",
                data.len(),
                self.bytes.len()
            )));
        }
        let before_hash = content_hash(&self.bytes);
        let mut updated = self.bytes.clone();
        updated[offset as usize..end as usize].copy_from_slice(data);
        let target = output.map(Path::to_path_buf).unwrap_or_else(|| self.path.clone());
        boost_fs::publish::publish_bytes_with_options(
            &target,
            &updated,
            PublishOptions { overwrite: OverwritePolicy::Replace, ..PublishOptions::default() },
        )
        .map_err(|error| InspectError::io(format!("cannot write {}: {error}", target.display())))?;
        let after_hash = content_hash(&updated);
        self.bytes = updated;
        Ok(PatchReport {
            path: target.display().to_string(),
            offset,
            length: data.len() as u64,
            before_hash,
            after_hash,
        })
    }

    /// Maps an RVA or virtual address onto a file offset.
    pub fn rva_to_offset(&self, address: u64) -> Result<u64> {
        match self.format {
            BinaryFormat::Pe => {
                let image = pe::parse_pe(&self.bytes).map_err(|error| InspectError::parse(format!("PE: {error}")))?;
                let rva = u32::try_from(address)
                    .map_err(|_| InspectError::range(format!("address 0x{address:x} does not fit a PE RVA")))?;
                // Headers occupy the bytes before the first section and are mapped 1:1.
                let first_section = image
                    .sections
                    .iter()
                    .map(|section| section.virtual_address)
                    .filter(|address| *address > 0)
                    .min()
                    .unwrap_or(0);
                if rva < first_section {
                    if u64::from(rva) < self.size() {
                        return Ok(u64::from(rva));
                    }
                    return Err(InspectError::range(format!("address 0x{address:x} maps beyond the end of the file")));
                }
                let offset = pe::rva_to_offset(&image.sections, rva)
                    .map_err(|error| InspectError::range(format!("address 0x{address:x}: {error}")))?;
                let offset = u64::from(offset);
                if offset >= self.size() {
                    return Err(InspectError::range(format!("address 0x{address:x} maps beyond the end of the file")));
                }
                Ok(offset)
            }
            BinaryFormat::Elf => {
                let image = elf::parse_elf(&self.bytes).map_err(|error| InspectError::parse(format!("ELF: {error}")))?;
                let offset = elf::address_to_offset(&image.sections, address)
                    .ok_or_else(|| InspectError::range(format!("address 0x{address:x} is not mapped by any section")))?;
                if offset >= self.size() {
                    return Err(InspectError::range(format!("address 0x{address:x} maps beyond the end of the file")));
                }
                Ok(offset)
            }
            BinaryFormat::MachO => Err(InspectError::unsupported("Mach-O address mapping is not implemented")),
            BinaryFormat::Unknown => Err(InspectError::unsupported("address mapping is only available for PE and ELF files")),
        }
    }
}

/// Detects the container from its leading bytes.
pub fn detect(bytes: &[u8]) -> BinaryFormat {
    if pe::parse_pe(bytes).is_ok() {
        return BinaryFormat::Pe;
    }
    if elf::parse_elf(bytes).is_ok() {
        return BinaryFormat::Elf;
    }
    if mach_o::macho_magic(bytes).is_some() {
        return BinaryFormat::MachO;
    }
    BinaryFormat::Unknown
}

/// Parses a hex byte string such as `90,90` or `0x90 0x90`.
pub fn parse_hex(text: &str) -> Result<Vec<u8>> {
    let mut digits = String::new();
    for token in text.split(|value: char| value == ',' || value == ':' || value == '-' || value.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let token = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X")).unwrap_or(token);
        if token.is_empty() || token.len() % 2 != 0 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(InspectError::parse(format!("`{token}` is not a hex byte string")));
        }
        digits.push_str(token);
    }
    if digits.is_empty() {
        return Err(InspectError::parse("no hex bytes were provided"));
    }
    Hex::any_case()
        .decode(digits.as_bytes())
        .map(|facts| facts.value)
        .map_err(|error| InspectError::parse(format!("invalid hex byte string: {error}")))
}

fn pe_magic(magic: u16) -> &'static str {
    match magic {
        0x010b => "PE32",
        0x020b => "PE32+",
        _ => "PE",
    }
}

fn pe_architecture(machine: u16) -> Architecture {
    match machine {
        0x014c => Architecture::X86,
        0x8664 => Architecture::X64,
        0xaa64 => Architecture::Arm64,
        _ => Architecture::Unknown,
    }
}

fn pe_section(section: &pe::SectionHeader) -> SectionInfo {
    const READ: u32 = 0x4000_0000;
    const WRITE: u32 = 0x8000_0000;
    const EXECUTE: u32 = 0x2000_0000;
    SectionInfo {
        name: section.name.clone(),
        virtual_address: u64::from(section.virtual_address),
        virtual_size: u64::from(section.virtual_size),
        raw_pointer: u64::from(section.pointer_to_raw_data),
        raw_size: u64::from(section.size_of_raw_data),
        characteristics: u64::from(section.characteristics),
        readable: section.characteristics & READ != 0,
        writable: section.characteristics & WRITE != 0,
        executable: section.characteristics & EXECUTE != 0,
    }
}

fn elf_section(section: &elf::ElfSection) -> SectionInfo {
    const WRITE: u64 = 0x1;
    const ALLOC: u64 = 0x2;
    const EXECUTE: u64 = 0x4;
    SectionInfo {
        name: section.name.clone(),
        virtual_address: section.address,
        virtual_size: section.size,
        raw_pointer: section.offset,
        raw_size: section.size,
        characteristics: section.flags,
        readable: section.flags & ALLOC != 0,
        writable: section.flags & WRITE != 0,
        executable: section.flags & EXECUTE != 0,
    }
}

fn macho_section(section: &mach_o::MachSection) -> SectionInfo {
    SectionInfo {
        name: if section.segment.is_empty() {
            section.name.clone()
        }
        else {
            format!("{},{}", section.segment, section.name)
        },
        virtual_address: section.address,
        virtual_size: section.size,
        raw_pointer: section.file_offset,
        raw_size: section.size,
        characteristics: u64::from(section.flags),
        readable: false,
        writable: false,
        executable: false,
    }
}

fn scan_strings(bytes: &[u8], min_len: usize) -> Vec<StringHit> {
    let minimum = min_len.max(1);
    let mut hits = Vec::new();
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let printable = (0x20..=0x7e).contains(&byte);
        match (printable, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                if index - begin >= minimum {
                    push_hit(&mut hits, begin, &bytes[begin..index]);
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start
        && bytes.len() - begin >= minimum
    {
        push_hit(&mut hits, begin, &bytes[begin..]);
    }
    hits
}

fn push_hit(hits: &mut Vec<StringHit>, offset: usize, bytes: &[u8]) {
    hits.push(StringHit {
        offset: offset as u64,
        length: bytes.len() as u64,
        value: String::from_utf8_lossy(bytes).into_owned(),
    });
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hash = XxHash64::new();
    hash.write(bytes);
    format!("{:016x}", hash.finish())
}

#[cfg(test)]
mod tests {
    use super::{Architecture, BinaryFormat, BinarySession, InspectOptions, parse_hex, scan_strings};

    #[test]
    fn hex_parser_accepts_common_separators() {
        assert_eq!(parse_hex("90,90").unwrap(), vec![0x90, 0x90]);
        assert_eq!(parse_hex("0x90 0x90").unwrap(), vec![0x90, 0x90]);
        assert_eq!(parse_hex("90:91-92").unwrap(), vec![0x90, 0x91, 0x92]);
        assert!(parse_hex("").is_err());
        assert!(parse_hex("9").is_err());
        assert!(parse_hex("zz").is_err());
    }

    #[test]
    fn strings_are_bounded_by_the_session_limit() {
        let session = BinarySession::with_options(
            "sample.bin".into(),
            b"\x00alpha\x00beta\x00gamma\x00".to_vec(),
            InspectOptions { strings_limit: Some(2), min_string_len: 3, ..InspectOptions::new("sample.bin") },
        );
        let report = session.strings(None, None);
        assert_eq!(report.total, 3);
        assert_eq!(report.hits.len(), 2);
        assert!(report.truncated);
        assert_eq!(report.hits[0].value, "alpha");
        assert_eq!(session.strings(Some(5), Some(1)).hits[0].value, "alpha");
    }

    #[test]
    fn hexdump_uses_the_configured_length() {
        let session = BinarySession::with_options(
            "sample.bin".into(),
            b"abcdefgh".to_vec(),
            InspectOptions { hexdump_length: 4, ..InspectOptions::new("sample.bin") },
        );
        let dump = session.hexdump(None, None).unwrap();
        assert!(dump.contains("61 62 63 64"));
        assert!(!dump.contains("65 66"));
        assert_eq!(session.read(0, 2).unwrap(), b"ab");
        assert_eq!(session.read(6, 4).unwrap_err().code(), "binary/range");
    }

    #[test]
    fn unknown_files_report_unknown_and_keep_operations_bounded() {
        let session = BinarySession::from_bytes("sample.bin", b"hello".to_vec());
        assert_eq!(session.format(), BinaryFormat::Unknown);
        assert_eq!(session.architecture(), Architecture::Unknown);
        let info = session.info().unwrap();
        assert_eq!(info.format, "unknown");
        assert_eq!(info.architecture, "unknown");
        assert_eq!(info.section_count, 0);
        assert_eq!(session.sections().unwrap_err().code(), "binary/unsupported");
        assert_eq!(session.rva_to_offset(0).unwrap_err().code(), "binary/unsupported");
    }

    #[test]
    fn patch_updates_the_session_and_reports_hashes() {
        let directory = std::env::temp_dir().join(format!("data-binary-inspect-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("patch.bin");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let mut session = BinarySession::open(InspectOptions::new(&path)).unwrap();
        let report = session.patch(2, &[0x41, 0x42], None).unwrap();
        assert_eq!(report.offset, 2);
        assert_eq!(report.length, 2);
        assert_ne!(report.before_hash, report.after_hash);
        assert_eq!(std::fs::read(&path).unwrap(), b"abABefgh");
        assert_eq!(session.read(0, 8).unwrap(), b"abABefgh");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn scanner_reports_offsets_and_honours_the_minimum() {
        let hits = scan_strings(b"\x00ab\x00abcd\x00", 3);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 4);
        assert_eq!(hits[0].value, "abcd");
    }
}
