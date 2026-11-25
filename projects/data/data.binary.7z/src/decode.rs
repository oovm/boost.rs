//! Folder decoding: coder chains over the internalized `data-compression` codecs.

use boost_source::binary::{ByteSource, ReadOutcome};
use data_binary_core::ByteExtent;
use data_compression::{CodecError, CodecLimits, Lzma1, Lzma2};

use crate::{
    format::*,
    model::{SevenZArchiveIndex, SevenZCoder, SevenZFolder},
};

/// Errors while decoding a 7z entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SevenZDecodeError {
    /// The requested entry index is outside the indexed entry list.
    EntryOutOfBounds {
        /// Requested entry index.
        entry: usize,
    },
    /// The entry has no data stream (directory or empty-stream file).
    EmptyStream,
    /// A coder method this crate does not decode.
    UnsupportedCoder {
        /// Decoder method id bytes from the header.
        method: Vec<u8>,
    },
    /// The pack stream range does not fit inside the source.
    PackOutOfBounds {
        /// Offending pack range.
        range: ByteExtent,
    },
    /// The source does not currently hold a required pack interval.
    NeedRange {
        /// Missing interval in the archive file space.
        range: ByteExtent,
    },
    /// A decoded size does not match the header declaration.
    SizeMismatch {
        /// Size declared by the header.
        expected: u64,
        /// Size actually produced.
        actual: u64,
    },
    /// The underlying codec failed.
    Codec(CodecError),
    /// The header model is inconsistent.
    Malformed(&'static str),
    /// The entry sub-stream range exceeds the decoded folder output.
    SubStreamOutOfBounds,
}

impl std::fmt::Display for SevenZDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntryOutOfBounds { entry } => write!(f, "entry index {entry} out of bounds"),
            Self::EmptyStream => write!(f, "entry has no data stream"),
            Self::UnsupportedCoder { method } => write!(f, "unsupported coder method {}", hex(method)),
            Self::PackOutOfBounds { range } => write!(f, "pack range {}..{} out of bounds", range.start, range.start + range.len),
            Self::NeedRange { range } => write!(f, "source does not hold bytes for range {}..{}", range.start, range.start + range.len),
            Self::SizeMismatch { expected, actual } => write!(f, "decoded size {actual} does not match declared size {expected}"),
            Self::Codec(error) => write!(f, "codec error: {error}"),
            Self::Malformed(message) => write!(f, "malformed folder model: {message}"),
            Self::SubStreamOutOfBounds => write!(f, "entry sub-stream exceeds the decoded folder output"),
        }
    }
}

impl std::error::Error for SevenZDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

/// Formats coder method bytes for diagnostics without allocating on the error path.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decodes one entry's payload, bounded by `limits`.
///
/// The entry's folder is decoded in full (solid archives share folder output), then the
/// entry's sub-stream slice is returned. Empty-stream entries decode to empty output.
pub fn decode_entry<S: ByteSource>(
    source: &S,
    index: &SevenZArchiveIndex,
    entry: usize,
    limits: CodecLimits,
) -> Result<Vec<u8>, SevenZDecodeError> {
    let entry = index.entries.get(entry).ok_or(SevenZDecodeError::EntryOutOfBounds { entry })?;
    let Some(folder_index) = entry.folder_index
    else {
        return Ok(Vec::new());
    };
    let folder = index.folders.get(folder_index).ok_or(SevenZDecodeError::Malformed("entry references a missing folder"))?;
    let decoded = decode_folder(source, folder, limits)?;
    let offset = entry.offset.unwrap_or(0) as usize;
    let size = entry.size.unwrap_or(0) as usize;
    let end = offset.checked_add(size).ok_or(SevenZDecodeError::SubStreamOutOfBounds)?;
    decoded.get(offset..end).map(<[u8]>::to_vec).ok_or(SevenZDecodeError::SubStreamOutOfBounds)
}

/// Decodes a folder: reads its pack streams, then applies the coder chain in order.
pub(crate) fn decode_folder<S: ByteSource>(source: &S, folder: &SevenZFolder, limits: CodecLimits) -> Result<Vec<u8>, SevenZDecodeError> {
    let length = source.length().ok_or(SevenZDecodeError::Malformed("source without length"))?;
    let mut current = Vec::new();
    for range in &folder.pack_ranges {
        let end = range.start.checked_add(range.len).ok_or(SevenZDecodeError::PackOutOfBounds { range: *range })?;
        if end > length {
            return Err(SevenZDecodeError::PackOutOfBounds { range: *range });
        }
        current.extend_from_slice(&read_pack(source, range)?);
    }
    for &coder_index in &folder.coder_order {
        let coder = folder.coders.get(coder_index).ok_or(SevenZDecodeError::Malformed("coder order references a missing coder"))?;
        let out_size = folder.unpack_sizes.get(coder_index).copied().ok_or(SevenZDecodeError::Malformed("coder without unpack size"))?;
        current = apply_coder(coder, current, out_size, limits)?;
    }
    Ok(current)
}

/// Reads one pack range into memory, reporting unfetched intervals as range needs.
fn read_pack<S: ByteSource>(source: &S, range: &ByteExtent) -> Result<Vec<u8>, SevenZDecodeError> {
    if range.len == 0 {
        return Ok(Vec::new());
    }
    let size = usize::try_from(range.len).map_err(|_| SevenZDecodeError::PackOutOfBounds { range: *range })?;
    let mut buffer = vec![0u8; size];
    match source.read_at(range.start, &mut buffer) {
        Ok(ReadOutcome::Complete) => Ok(buffer),
        Ok(ReadOutcome::Partial { bytes_read }) => Err(SevenZDecodeError::NeedRange {
            range: ByteExtent::new(range.start + bytes_read, range.len - bytes_read)
                .map_err(|_| SevenZDecodeError::PackOutOfBounds { range: *range })?,
        }),
        Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Unavailable { .. }) | Err(_) => Err(SevenZDecodeError::NeedRange { range: *range }),
    }
}

/// Applies one coder to the running buffer.
fn apply_coder(coder: &SevenZCoder, input: Vec<u8>, out_size: u64, limits: CodecLimits) -> Result<Vec<u8>, SevenZDecodeError> {
    match coder.method.as_slice() {
        CODER_COPY => {
            if input.len() as u64 != out_size {
                return Err(SevenZDecodeError::SizeMismatch { expected: out_size, actual: input.len() as u64 });
            }
            Ok(input)
        }
        CODER_LZMA => {
            let properties: [u8; 5] = coder
                .properties
                .get(..5)
                .ok_or(SevenZDecodeError::Malformed("LZMA coder without five property bytes"))?
                .try_into()
                .expect("fixed five-byte slice");
            let facts = Lzma1::decode_raw(&properties, &input, Some(out_size), limits).map_err(SevenZDecodeError::Codec)?;
            Ok(facts.value)
        }
        CODER_LZMA2 => {
            let facts = Lzma2::decode(&input, limits).map_err(SevenZDecodeError::Codec)?;
            if facts.value.len() as u64 != out_size {
                return Err(SevenZDecodeError::SizeMismatch { expected: out_size, actual: facts.value.len() as u64 });
            }
            Ok(facts.value)
        }
        CODER_BCJ_X86 => {
            let mut buffer = input;
            bcj_x86_decode(&mut buffer);
            Ok(buffer)
        }
        other => Err(SevenZDecodeError::UnsupportedCoder { method: other.to_vec() }),
    }
}

/// Decodes the BCJ x86 branch filter in place.
///
/// Mechanical port of the LZMA SDK `C/Bra86.c` (`z7_BranchConvSt_X86_Dec`, 2023-04-02)
/// with `pc = 0` and the initial state of `Z7_BRANCH_CONV_ST_X86_STATE_INIT_VAL`.
/// Call and jump displacements (`0xE8`/`0xE9`) whose 32-bit value sits in the centered
/// 25-bit range are rewritten from absolute addresses back to relative displacements.
pub fn bcj_x86_decode(data: &mut [u8]) {
    if data.len() < 5 {
        return;
    }
    let lim = data.len() - 4;
    let mut p = 0usize;
    let mut mask: u32 = 0;
    // The SDK starts with `pc = 0`, adds 4 for the next-instruction offset, and folds
    // the buffer base away, so `BR_PC_GET` is `4 + p` for a buffer that starts at 0.
    let pc: u32 = 4;

    'start: loop {
        if p >= lim {
            return;
        }
        // Scan the four-byte window, shifting the overlap mask past non-branch bytes.
        let mut n = 0usize;
        while n < 4 && !is_bcj_byte(data[p + n]) {
            if n == 2 {
                mask = 0;
            }
            else {
                mask >>= 1;
            }
            n += 1;
        }
        if n < 4 {
            // The SDK leaves `p` on the displacement field (opcode byte + 1).
            p += n + 1;
            if n < 3 && mask != 0 {
                // A branch byte inside the previous displacement window: consult the
                // overlap mask before trusting it.
                if p > lim {
                    return;
                }
                if mask > 4 || mask == 3 {
                    mask = (mask >> 1) | 4;
                    continue 'start;
                }
                mask >>= 1;
                if need_conv_for_ms_byte(data[p + mask as usize]) {
                    mask |= 4;
                    continue 'start;
                }
                let mut value = read_u32_le(data, p).wrapping_add(1 << 24);
                if value & 0xFE00_0000 != 0 {
                    mask |= 4;
                    continue 'start;
                }
                value = value.wrapping_sub(pc.wrapping_add(p as u32));
                let shift = mask << 3;
                if need_conv_for_ms_byte((value >> shift) as u8) {
                    value ^= (0x100u32 << shift) - 1;
                    value = value.wrapping_sub(pc.wrapping_add(p as u32));
                }
                mask = 0;
                write_u32_le(data, p, normalize(value));
                p += 4;
            }
            else {
                // `a3`: the branch byte is trusted, convert directly.
                if p > lim {
                    return;
                }
                let mut value = read_u32_le(data, p).wrapping_add(1 << 24);
                if value & 0xFE00_0000 != 0 {
                    mask |= 4;
                    continue 'start;
                }
                value = value.wrapping_sub(pc.wrapping_add(p as u32));
                write_u32_le(data, p, normalize(value));
                p += 4;
            }
        }
        // `main_loop`: steady-state scan without mask consultation.
        loop {
            if p >= lim {
                return;
            }
            let mut n = 0usize;
            while n < 4 && !is_bcj_byte(data[p + n]) {
                n += 1;
            }
            if n < 4 {
                p += n + 1;
                if p > lim {
                    return;
                }
                let mut value = read_u32_le(data, p).wrapping_add(1 << 24);
                if value & 0xFE00_0000 != 0 {
                    mask |= 4;
                    continue 'start;
                }
                value = value.wrapping_sub(pc.wrapping_add(p as u32));
                write_u32_le(data, p, normalize(value));
                p += 4;
            }
            else {
                p += 4;
            }
        }
    }
}

/// Whether the byte is a call (`0xE8`) or jump (`0xE9`) opcode.
fn is_bcj_byte(byte: u8) -> bool {
    byte & 0xFE == 0xE8
}

/// Whether a high address byte marks a convertible absolute address (`0x00` or `0xFF`).
fn need_conv_for_ms_byte(byte: u8) -> bool {
    byte.wrapping_add(1) & 0xFE == 0
}

/// Folds a converted value back into the centered 25-bit signed range.
fn normalize(value: u32) -> u32 {
    (value & ((1 << 25) - 1)).wrapping_sub(1 << 24)
}

fn read_u32_le(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("fixed four-byte window"))
}

fn write_u32_le(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
