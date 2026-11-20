//! The fixed 96-byte header defined by contract 42 §2.
//!
//! All integers are little-endian unsigned. The header is parsed field by
//! field; it is never transmuted into a Rust struct.

use crate::error::XpuError;
use crate::index::{KNOWN_REQUIRED_FLAGS, ENTRY_BYTES};

/// Magic: `XPU\0` + CR LF SUB LF.
pub const MAGIC: [u8; 8] = [0x58, 0x50, 0x55, 0x00, 0x0D, 0x0A, 0x1A, 0x0A];

/// Size of the fixed header.
pub const HEADER_BYTES: usize = 96;

/// Size of one directory entry.
pub const DIRECTORY_ENTRY_BYTES: usize = ENTRY_BYTES;

/// Format version implemented by this crate.
pub const FORMAT_MAJOR: u16 = 1;
pub const FORMAT_MINOR: u16 = 0;

/// Parsed view of the fixed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub major: u16,
    pub minor: u16,
    pub required_flags: u32,
    pub section_count: u32,
    pub directory_offset: u64,
    pub file_bytes: u64,
    pub manifest_id: u32,
    pub directory_sha256: [u8; 32],
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Parse and verify a header against the actual file length.
pub fn parse(bytes: &[u8], actual_len: u64) -> Result<Header, XpuError> {
    if bytes.len() < HEADER_BYTES {
        return Err(XpuError::TooShort { need: HEADER_BYTES as u64, have: bytes.len() as u64 });
    }
    if bytes[0..8] != MAGIC {
        return Err(XpuError::BadMagic);
    }
    let major = u16_at(bytes, 8);
    let minor = u16_at(bytes, 10);
    if major != FORMAT_MAJOR || minor != FORMAT_MINOR {
        return Err(XpuError::UnsupportedVersion { major, minor });
    }
    if u16_at(bytes, 12) != HEADER_BYTES as u16 {
        return Err(XpuError::HeaderField("header_bytes must be 96"));
    }
    if u16_at(bytes, 14) != ENTRY_BYTES as u16 {
        return Err(XpuError::HeaderField("entry_bytes must be 80"));
    }
    let required_flags = u32_at(bytes, 16);
    if required_flags & !KNOWN_REQUIRED_FLAGS != 0 {
        return Err(XpuError::UnknownRequiredFlags(required_flags));
    }
    let section_count = u32_at(bytes, 20);
    let directory_offset = u64_at(bytes, 24);
    if directory_offset != HEADER_BYTES as u64 {
        return Err(XpuError::HeaderField("directory_offset is fixed at 96 in v1"));
    }
    let file_bytes = u64_at(bytes, 32);
    if file_bytes != actual_len {
        return Err(XpuError::FileLengthMismatch { declared: file_bytes, actual: actual_len });
    }
    let manifest_id = u32_at(bytes, 40);
    if u32_at(bytes, 44) != 0 || bytes[80..96] != [0u8; 16] {
        return Err(XpuError::ReservedNonZero);
    }
    let mut directory_sha256 = [0u8; 32];
    directory_sha256.copy_from_slice(&bytes[48..80]);
    Ok(Header { major, minor, required_flags, section_count, directory_offset, file_bytes, manifest_id, directory_sha256 })
}

/// Serialise a header; reserved fields are written as zero.
pub fn write(h: &Header) -> Vec<u8> {
    let mut b = vec![0u8; HEADER_BYTES];
    b[0..8].copy_from_slice(&MAGIC);
    b[8..10].copy_from_slice(&h.major.to_le_bytes());
    b[10..12].copy_from_slice(&h.minor.to_le_bytes());
    b[12..14].copy_from_slice(&(HEADER_BYTES as u16).to_le_bytes());
    b[14..16].copy_from_slice(&(ENTRY_BYTES as u16).to_le_bytes());
    b[16..20].copy_from_slice(&h.required_flags.to_le_bytes());
    b[20..24].copy_from_slice(&h.section_count.to_le_bytes());
    b[24..32].copy_from_slice(&h.directory_offset.to_le_bytes());
    b[32..40].copy_from_slice(&h.file_bytes.to_le_bytes());
    b[40..44].copy_from_slice(&h.manifest_id.to_le_bytes());
    b[48..80].copy_from_slice(&h.directory_sha256);
    b
}
