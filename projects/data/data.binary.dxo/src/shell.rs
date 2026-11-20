//! Fixed shells: the 96-byte main header and the 64-byte volume header.

use crate::error::DxoError;

/// Magic of the main file: `DXO\0` + CR LF SUB LF.
pub const MAIN_MAGIC: [u8; 8] = [0x44, 0x58, 0x4F, 0x00, 0x0D, 0x0A, 0x1A, 0x0A];

/// Magic of a numbered volume: `DXOV` + CR LF SUB LF.
pub const VOLUME_MAGIC: [u8; 8] = [0x44, 0x58, 0x4F, 0x56, 0x0D, 0x0A, 0x1A, 0x0A];

/// Size of the main file header.
pub const MAIN_HEADER_BYTES: usize = 96;

/// Size of a volume header.
pub const VOLUME_HEADER_BYTES: usize = 64;

/// Parsed view of a main file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainHeader {
    pub major: u16,
    pub minor: u16,
    pub index_bytes: u64,
    pub file_bytes: u64,
    pub index_sha256: [u8; 32],
}

/// Parsed view of a volume header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeHeader {
    pub major: u16,
    pub minor: u16,
    pub volume_index: u32,
    pub file_bytes: u64,
    pub index_sha256: [u8; 32],
}

fn check_version(major: u16, minor: u16) -> Result<(), DxoError> {
    if major != crate::index::FORMAT_MAJOR || minor != crate::index::FORMAT_MINOR {
        return Err(DxoError::UnsupportedVersion { major, minor });
    }
    Ok(())
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

/// Parse and verify a main header against the actual file length.
pub fn parse_main(bytes: &[u8], actual_len: u64) -> Result<MainHeader, DxoError> {
    if bytes.len() < MAIN_HEADER_BYTES {
        return Err(DxoError::TooShort { need: MAIN_HEADER_BYTES as u64, have: bytes.len() as u64 });
    }
    if bytes[0..8] != MAIN_MAGIC {
        return Err(DxoError::BadMagic);
    }
    let major = u16_at(bytes, 8);
    let minor = u16_at(bytes, 10);
    check_version(major, minor)?;
    if u16_at(bytes, 12) != MAIN_HEADER_BYTES as u16 {
        return Err(DxoError::Range("main header_bytes field is wrong"));
    }
    if u16_at(bytes, 14) != 0 || bytes[64..96] != [0u8; 32] {
        return Err(DxoError::ReservedNonZero);
    }
    let index_bytes = u64_at(bytes, 16);
    let file_bytes = u64_at(bytes, 24);
    if file_bytes != actual_len {
        return Err(DxoError::TooShort { need: file_bytes, have: actual_len });
    }
    let floor = MAIN_HEADER_BYTES as u64;
    let index_end = index_bytes.checked_add(floor).ok_or(DxoError::Range("index length overflow"))?;
    if index_end > file_bytes {
        return Err(DxoError::TooShort { need: index_end, have: file_bytes });
    }
    let mut index_sha256 = [0u8; 32];
    index_sha256.copy_from_slice(&bytes[32..64]);
    Ok(MainHeader { major, minor, index_bytes, file_bytes, index_sha256 })
}

/// Parse and verify a volume header against the actual file length.
pub fn parse_volume(bytes: &[u8], actual_len: u64) -> Result<VolumeHeader, DxoError> {
    if bytes.len() < VOLUME_HEADER_BYTES {
        return Err(DxoError::TooShort { need: VOLUME_HEADER_BYTES as u64, have: bytes.len() as u64 });
    }
    if bytes[0..8] != VOLUME_MAGIC {
        return Err(DxoError::BadMagic);
    }
    let major = u16_at(bytes, 8);
    let minor = u16_at(bytes, 10);
    check_version(major, minor)?;
    if u16_at(bytes, 12) != VOLUME_HEADER_BYTES as u16 {
        return Err(DxoError::Range("volume header_bytes field is wrong"));
    }
    if u16_at(bytes, 14) != 0 || u32_at(bytes, 20) != 0 {
        return Err(DxoError::ReservedNonZero);
    }
    let volume_index = u32_at(bytes, 16);
    let file_bytes = u64_at(bytes, 24);
    if file_bytes != actual_len {
        return Err(DxoError::TooShort { need: file_bytes, have: actual_len });
    }
    if file_bytes < VOLUME_HEADER_BYTES as u64 {
        return Err(DxoError::Range("volume shorter than its header"));
    }
    let mut index_sha256 = [0u8; 32];
    index_sha256.copy_from_slice(&bytes[32..64]);
    Ok(VolumeHeader { major, minor, volume_index, file_bytes, index_sha256 })
}

/// Write a main header. `index_sha256` covers the raw index bytes.
pub fn write_main(index_bytes: u64, file_bytes: u64, index_sha256: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; MAIN_HEADER_BYTES];
    b[0..8].copy_from_slice(&MAIN_MAGIC);
    b[8..10].copy_from_slice(&crate::index::FORMAT_MAJOR.to_le_bytes());
    b[10..12].copy_from_slice(&crate::index::FORMAT_MINOR.to_le_bytes());
    b[12..14].copy_from_slice(&(MAIN_HEADER_BYTES as u16).to_le_bytes());
    b[16..24].copy_from_slice(&index_bytes.to_le_bytes());
    b[24..32].copy_from_slice(&file_bytes.to_le_bytes());
    b[32..64].copy_from_slice(index_sha256);
    b
}

/// Write a volume header. `index_sha256` repeats the main index digest.
pub fn write_volume(volume_index: u32, file_bytes: u64, index_sha256: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; VOLUME_HEADER_BYTES];
    b[0..8].copy_from_slice(&VOLUME_MAGIC);
    b[8..10].copy_from_slice(&crate::index::FORMAT_MAJOR.to_le_bytes());
    b[10..12].copy_from_slice(&crate::index::FORMAT_MINOR.to_le_bytes());
    b[12..14].copy_from_slice(&(VOLUME_HEADER_BYTES as u16).to_le_bytes());
    b[16..20].copy_from_slice(&volume_index.to_le_bytes());
    b[24..32].copy_from_slice(&file_bytes.to_le_bytes());
    b[32..64].copy_from_slice(index_sha256);
    b
}

/// Derive the volume file name for a main asset path.
/// `main` must end in `.dxo`; the volume is `<main>.<zero-padded index>`.
pub fn volume_path(main: &std::path::Path, volume_width: u64, volume_index: u64) -> std::path::PathBuf {
    let name = format!("{:0>width$}", volume_index, width = volume_width as usize);
    let mut p = main.as_os_str().to_os_string();
    p.push(".");
    p.push(name);
    std::path::PathBuf::from(p)
}
