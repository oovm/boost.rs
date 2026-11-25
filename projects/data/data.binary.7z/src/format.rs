//! Format constants for the 7z container (signature header, property ids, coder ids).

/// Magic bytes at the start of every 7z container.
pub(crate) const SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
/// Total length of the signature header (magic + version + crc + start header).
pub(crate) const SIGNATURE_HEADER_LEN: u64 = 32;

/// End of the current property tree level.
pub(crate) const K_END: u8 = 0x00;
/// Main header root property.
pub(crate) const K_HEADER: u8 = 0x01;
/// Archive properties (skipped).
pub(crate) const K_ARCHIVE_PROPERTIES: u8 = 0x02;
/// Main streams info property.
pub(crate) const K_MAIN_STREAMS_INFO: u8 = 0x04;
/// Files info property.
pub(crate) const K_FILES_INFO: u8 = 0x05;
/// Pack info property.
pub(crate) const K_PACK_INFO: u8 = 0x06;
/// Unpack info property.
pub(crate) const K_UNPACK_INFO: u8 = 0x07;
/// Sub-streams info property.
pub(crate) const K_SUB_STREAMS_INFO: u8 = 0x08;
/// Size list property.
pub(crate) const K_SIZE: u8 = 0x09;
/// CRC list property.
pub(crate) const K_CRC: u8 = 0x0A;
/// Folder list property.
pub(crate) const K_FOLDER: u8 = 0x0B;
/// Per-coder unpack size property.
pub(crate) const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
/// Sub-stream count property.
pub(crate) const K_NUM_UNPACK_STREAM: u8 = 0x0D;
/// Empty-stream bitmap property.
pub(crate) const K_EMPTY_STREAM: u8 = 0x0E;
/// Empty-file bitmap property.
pub(crate) const K_EMPTY_FILE: u8 = 0x0F;
/// Anti-item bitmap property.
pub(crate) const K_ANTI: u8 = 0x10;
/// File name property.
pub(crate) const K_NAME: u8 = 0x11;
/// Creation time property.
pub(crate) const K_C_TIME: u8 = 0x12;
/// Access time property.
pub(crate) const K_A_TIME: u8 = 0x13;
/// Modified time property.
pub(crate) const K_M_TIME: u8 = 0x14;
/// Windows attributes property.
pub(crate) const K_WIN_ATTRIBUTES: u8 = 0x15;
/// Encoded (compressed) header property.
pub(crate) const K_ENCODED_HEADER: u8 = 0x17;
/// Dummy padding property.
pub(crate) const K_DUMMY: u8 = 0x19;

/// Coder id for stored (uncompressed) data.
pub(crate) const CODER_COPY: &[u8] = &[0x00];
/// Coder id for raw LZMA1.
pub(crate) const CODER_LZMA: &[u8] = &[0x03, 0x01, 0x01];
/// Coder id for LZMA2.
pub(crate) const CODER_LZMA2: &[u8] = &[0x21];
/// Coder id for the BCJ x86 branch filter.
pub(crate) const CODER_BCJ_X86: &[u8] = &[0x03, 0x03, 0x01, 0x03];

/// Largest coder count accepted for one folder.
pub(crate) const MAX_BLOCK_CODERS: usize = 64;
