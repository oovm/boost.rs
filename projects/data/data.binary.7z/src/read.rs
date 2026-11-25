//! 7z header parsing: packed integers, bitmaps, and the header property tree.

use boost_source::binary::{ByteSource, ReadOutcome};
use data_binary_core::ByteExtent;
use data_compression::CodecLimits;

use crate::{
    decode::decode_folder,
    format::*,
    model::{SevenZArchiveIndex, SevenZCoder, SevenZEntry, SevenZFolder},
};

/// Errors while indexing a 7z container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SevenZIndexError {
    /// The input does not start with the 7z signature header.
    NotSevenZ,
    /// The container declares a major version this parser does not read.
    UnsupportedVersion {
        /// Declared major version byte.
        major: u8,
    },
    /// The declared next-header range does not fit inside the source.
    HeaderOutOfBounds,
    /// The stored start-header digest does not match its record bytes.
    StartHeaderCrcMismatch {
        /// Digest stored in the signature header.
        expected: u32,
        /// Digest computed over the start-header record.
        actual: u32,
    },
    /// A header property is truncated.
    Truncated {
        /// Absolute offset of the missing byte.
        offset: u64,
        /// Number of bytes the property still needs.
        needed: usize,
    },
    /// A header property violates the container structure.
    Malformed(&'static str),
    /// A declared count exceeds what the remaining header bytes could supply.
    CountTooLarge {
        /// Absolute offset of the offending count.
        offset: u64,
        /// Declared count value.
        count: u64,
    },
    /// The encoded header could not be decoded.
    EncodedHeader(String),
    /// The source does not currently hold a required interval.
    NeedRange {
        /// Missing interval in the archive file space.
        range: ByteExtent,
    },
}

impl std::fmt::Display for SevenZIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSevenZ => write!(f, "7z signature not found"),
            Self::UnsupportedVersion { major } => write!(f, "unsupported 7z major version {major}"),
            Self::HeaderOutOfBounds => write!(f, "next header out of bounds"),
            Self::StartHeaderCrcMismatch { expected, actual } => {
                write!(f, "start header crc mismatch: expected {expected:#010x}, actual {actual:#010x}")
            }
            Self::Truncated { offset, needed } => write!(f, "truncated header property at offset {offset}, needs {needed} more bytes"),
            Self::Malformed(message) => write!(f, "malformed header: {message}"),
            Self::CountTooLarge { offset, count } => write!(f, "header count {count} at offset {offset} exceeds available input"),
            Self::EncodedHeader(message) => write!(f, "encoded header decode failed: {message}"),
            Self::NeedRange { range } => write!(f, "source does not hold bytes for range {}..{}", range.start, range.start + range.len),
        }
    }
}

impl std::error::Error for SevenZIndexError {}

/// Cursor over an in-memory header buffer with absolute-offset reporting.
struct Cursor<'a> {
    data: &'a [u8],
    base: u64,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], base: u64) -> Self {
        Self { data, base, pos: 0 }
    }

    fn absolute(&self, pos: usize) -> u64 {
        self.base + pos as u64
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn u8(&mut self) -> Result<u8, SevenZIndexError> {
        let byte = *self.data.get(self.pos).ok_or(SevenZIndexError::Truncated { offset: self.absolute(self.pos), needed: 1 })?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], SevenZIndexError> {
        let slice =
            self.data.get(self.pos..self.pos + len).ok_or(SevenZIndexError::Truncated { offset: self.absolute(self.pos), needed: len })?;
        self.pos += len;
        Ok(slice)
    }

    fn u32_le(&mut self) -> Result<u32, SevenZIndexError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64_le(&mut self) -> Result<u64, SevenZIndexError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("fixed eight-byte field")))
    }

    /// Reads the variable-length packed unsigned integer of the 7z header format.
    fn packed_u64(&mut self) -> Result<u64, SevenZIndexError> {
        let first = self.u8()?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                return Ok(value | (u64::from(first & (mask - 1)) << (8 * i)));
            }
            let byte = self.u8()?;
            value |= u64::from(byte) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    /// Reads a packed count and bounds it by the remaining header bytes: every counted
    /// element consumes at least one more header byte, so a larger count cannot be real.
    fn bounded_count(&mut self) -> Result<usize, SevenZIndexError> {
        let offset = self.absolute(self.pos);
        let value = self.packed_u64()?;
        if value > self.remaining() as u64 {
            return Err(SevenZIndexError::CountTooLarge { offset, count: value });
        }
        Ok(value as usize)
    }

    /// Reads `count` bitmap bits, most significant bit first.
    fn bits(&mut self, count: usize) -> Result<Vec<bool>, SevenZIndexError> {
        let mut out = vec![false; count];
        let mut cache = 0u8;
        let mut left = 0u8;
        for slot in out.iter_mut() {
            if left == 0 {
                cache = self.u8()?;
                left = 8;
            }
            *slot = cache & 0x80 != 0;
            cache <<= 1;
            left -= 1;
        }
        Ok(out)
    }

    /// Reads the all-or-bits pattern: one flag byte, then either all-defined or a bitmap.
    fn all_or_bits(&mut self, count: usize) -> Result<Vec<bool>, SevenZIndexError> {
        if self.u8()? != 0 {
            return Ok(vec![true; count]);
        }
        self.bits(count)
    }
}

/// Sub-stream split of the folder outputs, from `SubStreamsInfo`.
struct SubStreams {
    unpack_sizes: Vec<u64>,
    has_crc: Vec<bool>,
    crcs: Vec<u32>,
}

/// Parsed streams info: folders plus the optional sub-stream split.
struct StreamsInfo {
    folders: Vec<SevenZFolder>,
    sub_streams: Option<SubStreams>,
}

/// Indexes `source` when it contains a 7z container.
///
/// Only the signature header and the (possibly encoded) next header are read while
/// indexing; payload bytes stay on the source until an entry is decoded.
pub fn index_7z<S: ByteSource>(source: &S) -> Result<SevenZArchiveIndex, SevenZIndexError> {
    let length = source.length().ok_or(SevenZIndexError::NotSevenZ)?;
    if length < SIGNATURE_HEADER_LEN {
        return Err(SevenZIndexError::NotSevenZ);
    }
    let head = read_range(source, 0, SIGNATURE_HEADER_LEN)?;
    if head[..6] != SIGNATURE {
        return Err(SevenZIndexError::NotSevenZ);
    }
    let version = (head[6], head[7]);
    if head[6] != 0 {
        return Err(SevenZIndexError::UnsupportedVersion { major: head[6] });
    }
    let expected_crc = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let record = &head[12..32];
    if expected_crc == 0 && record.iter().all(|byte| *byte == 0) {
        return Ok(SevenZArchiveIndex::empty(version));
    }
    let actual_crc = data_checksum::crc32(record);
    if actual_crc != expected_crc {
        return Err(SevenZIndexError::StartHeaderCrcMismatch { expected: expected_crc, actual: actual_crc });
    }
    let next_offset = SIGNATURE_HEADER_LEN.checked_add(u64::from_le_bytes(record[0..8].try_into().expect("fixed eight-byte field")));
    let next_size = u64::from_le_bytes(record[8..16].try_into().expect("fixed eight-byte field"));
    let Some(next_offset) = next_offset.filter(|offset| offset.saturating_add(next_size) <= length)
    else {
        return Err(SevenZIndexError::HeaderOutOfBounds);
    };
    let header = read_range(source, next_offset, next_size)?;
    let mut cursor = Cursor::new(&header, next_offset);
    let property = cursor.u8()?;
    if property == K_ENCODED_HEADER {
        // The header itself lives in a packed stream: parse its streams info, decode
        // the single folder, and re-enter parsing on the decoded header bytes. The
        // encoded header's own pack positions sit right after the encoded region.
        let encoded = parse_streams_info(&mut cursor, next_offset + header.len() as u64)?;
        if encoded.folders.len() != 1 {
            return Err(SevenZIndexError::Malformed("encoded header without exactly one folder"));
        }
        let folder = &encoded.folders[0];
        let owned = decode_folder(source, folder, CodecLimits::new()).map_err(|error| SevenZIndexError::EncodedHeader(error.to_string()))?;
        let mut inner = Cursor::new(&owned, 0);
        if inner.u8()? != K_HEADER {
            return Err(SevenZIndexError::Malformed("encoded header does not yield a header"));
        }
        return parse_main_header(&mut inner, version, next_offset + header.len() as u64);
    }
    if property != K_HEADER {
        return Err(SevenZIndexError::Malformed("next header without a header property"));
    }
    parse_main_header(&mut cursor, version, SIGNATURE_HEADER_LEN)
}

/// Parses the `StreamsInfo` + `FilesInfo` body of a header, cursor past `K_HEADER`.
fn parse_main_header(cursor: &mut Cursor<'_>, version: (u8, u8), pack_base: u64) -> Result<SevenZArchiveIndex, SevenZIndexError> {
    if cursor.peek() == Some(K_ARCHIVE_PROPERTIES) {
        // Archive-level properties are size-prefixed blobs; the index does not need them.
        let _ = cursor.u8()?;
        let size = cursor.bounded_count()?;
        let _ = cursor.take(size)?;
    }
    let streams = if cursor.peek() == Some(K_MAIN_STREAMS_INFO) {
        let _ = cursor.u8()?;
        Some(parse_streams_info(cursor, pack_base)?)
    }
    else {
        None
    };
    let mut folders = streams.as_ref().map(|info| info.folders.clone()).unwrap_or_default();
    let sub_streams = streams.as_ref().and_then(|info| info.sub_streams.as_ref());
    let entries = if cursor.peek() == Some(K_FILES_INFO) {
        let _ = cursor.u8()?;
        parse_files_info(cursor, &mut folders, sub_streams)?
    }
    else {
        Vec::new()
    };
    let mut methods: Vec<Vec<u8>> = Vec::new();
    let mut total_unpack_size = 0u64;
    for folder in &folders {
        total_unpack_size =
            total_unpack_size.checked_add(folder.unpack_size).ok_or(SevenZIndexError::Malformed("total unpack size overflow"))?;
        for coder in &folder.coders {
            if !methods.contains(&coder.method) {
                methods.push(coder.method.clone());
            }
        }
    }
    Ok(SevenZArchiveIndex { version, methods, folders, entries, total_unpack_size })
}

/// Parses `PackInfo`, `UnpackInfo`, and `SubStreamsInfo` from the current cursor.
///
/// `pack_base` is the absolute file offset that `PackInfo` positions are relative to.
fn parse_streams_info(cursor: &mut Cursor<'_>, pack_base: u64) -> Result<StreamsInfo, SevenZIndexError> {
    let mut pack_pos = 0u64;
    let mut pack_sizes: Vec<u64> = Vec::new();
    let mut folders: Vec<SevenZFolder> = Vec::new();
    let mut sub_streams: Option<SubStreams> = None;

    if cursor.peek() == Some(K_PACK_INFO) {
        let _ = cursor.u8()?;
        pack_pos = cursor.packed_u64()?;
        let num_pack_streams = cursor.bounded_count()?;
        if cursor.peek() == Some(K_SIZE) {
            let _ = cursor.u8()?;
            pack_sizes = Vec::with_capacity(num_pack_streams);
            for _ in 0..num_pack_streams {
                pack_sizes.push(cursor.packed_u64()?);
            }
        }
        if cursor.peek() == Some(K_CRC) {
            let _ = cursor.u8()?;
            let defined = cursor.all_or_bits(num_pack_streams)?;
            for defined in defined {
                if defined {
                    let _ = cursor.u32_le()?;
                }
            }
        }
        if cursor.u8()? != K_END {
            return Err(SevenZIndexError::Malformed("pack info not terminated"));
        }
    }

    if cursor.peek() == Some(K_UNPACK_INFO) {
        let _ = cursor.u8()?;
        if cursor.u8()? != K_FOLDER {
            return Err(SevenZIndexError::Malformed("unpack info without folder list"));
        }
        let num_folders = cursor.bounded_count()?;
        if cursor.u8()? != 0 {
            return Err(SevenZIndexError::Malformed("external folder data is not supported"));
        }
        for _ in 0..num_folders {
            folders.push(parse_folder(cursor)?);
        }
        if cursor.u8()? != K_CODERS_UNPACK_SIZE {
            return Err(SevenZIndexError::Malformed("folder list without coder unpack sizes"));
        }
        for folder in &mut folders {
            folder.unpack_sizes = Vec::with_capacity(folder.coders.len());
            for _ in 0..folder.coders.len() {
                folder.unpack_sizes.push(cursor.packed_u64()?);
            }
            // The folder output is the unbound coder output: the last coder in the chain.
            let last = folder.coder_order.last().copied().ok_or(SevenZIndexError::Malformed("folder without coders"))?;
            folder.unpack_size = folder.unpack_sizes[last];
        }
        if cursor.peek() == Some(K_CRC) {
            let _ = cursor.u8()?;
            let defined = cursor.all_or_bits(num_folders)?;
            for (folder, defined) in folders.iter_mut().zip(defined) {
                if defined {
                    folder.crc = Some(cursor.u32_le()?);
                }
            }
        }
        if cursor.u8()? != K_END {
            return Err(SevenZIndexError::Malformed("unpack info not terminated"));
        }
    }

    if cursor.peek() == Some(K_SUB_STREAMS_INFO) {
        let _ = cursor.u8()?;
        sub_streams = Some(parse_sub_streams_info(cursor, &mut folders)?);
        if cursor.u8()? != K_END {
            return Err(SevenZIndexError::Malformed("sub-streams info not terminated"));
        }
    }

    if cursor.u8()? != K_END {
        return Err(SevenZIndexError::Malformed("streams info not terminated"));
    }

    // Resolve absolute pack ranges now that pack positions and sizes are known.
    let mut stream_index = 0usize;
    for folder in &mut folders {
        let mut ranges = Vec::with_capacity(folder.pack_stream_count);
        for _ in 0..folder.pack_stream_count {
            let size = *pack_sizes.get(stream_index).ok_or(SevenZIndexError::Malformed("folder references a missing pack stream"))?;
            let start = pack_base
                .checked_add(pack_pos)
                .and_then(|base| base.checked_add(offset_of(&pack_sizes[..stream_index])))
                .ok_or(SevenZIndexError::Malformed("pack range overflow"))?;
            ranges.push(ByteExtent::new(start, size).map_err(|_| SevenZIndexError::Malformed("pack range out of bounds"))?);
            stream_index += 1;
        }
        folder.pack_ranges = ranges;
    }

    Ok(StreamsInfo { folders, sub_streams })
}

/// Sums pack stream sizes before `stream_index`, the offset of that stream.
fn offset_of(sizes: &[u64]) -> u64 {
    sizes.iter().fold(0u64, |sum, size| sum.saturating_add(*size))
}

/// Parses one folder (block): coders, bind pairs, and packed stream indices.
fn parse_folder(cursor: &mut Cursor<'_>) -> Result<SevenZFolder, SevenZIndexError> {
    let num_coders = cursor.bounded_count()?;
    if num_coders > MAX_BLOCK_CODERS {
        return Err(SevenZIndexError::Malformed("folder has too many coders"));
    }
    let mut coders = Vec::with_capacity(num_coders);
    let mut total_in_streams = 0u64;
    let mut total_out_streams = 0u64;
    for _ in 0..num_coders {
        let bits = cursor.u8()?;
        let id_size = (bits & 0xF) as usize;
        let is_simple = bits & 0x10 == 0;
        let has_attributes = bits & 0x20 != 0;
        if bits & 0x80 != 0 {
            return Err(SevenZIndexError::Malformed("alternative coder methods are not supported"));
        }
        let method = cursor.take(id_size)?.to_vec();
        let (num_in_streams, num_out_streams) = if is_simple { (1u64, 1u64) } else { (cursor.packed_u64()?, cursor.packed_u64()?) };
        if num_out_streams != 1 {
            return Err(SevenZIndexError::Malformed("coder must have exactly one output stream"));
        }
        let properties = if has_attributes {
            let size = cursor.bounded_count()?;
            cursor.take(size)?.to_vec()
        }
        else {
            Vec::new()
        };
        total_in_streams = total_in_streams.checked_add(num_in_streams).ok_or(SevenZIndexError::Malformed("coder stream count overflow"))?;
        total_out_streams += 1;
        coders.push(SevenZCoder { method, properties, num_in_streams, num_out_streams });
    }
    if total_out_streams == 0 || total_in_streams > 1_048_576 {
        return Err(SevenZIndexError::Malformed("folder stream counts out of range"));
    }
    let num_bind_pairs = (total_out_streams - 1) as usize;
    let mut bind_pairs = Vec::with_capacity(num_bind_pairs);
    let mut in_stream_used = vec![false; total_in_streams as usize];
    for _ in 0..num_bind_pairs {
        let in_index = cursor.packed_u64()?;
        let out_index = cursor.packed_u64()?;
        if in_index >= total_in_streams || out_index >= total_out_streams || in_stream_used[in_index as usize] {
            return Err(SevenZIndexError::Malformed("bind pair references an out-of-range or used stream"));
        }
        in_stream_used[in_index as usize] = true;
        bind_pairs.push((in_index, out_index));
    }
    let num_packed_streams = (total_in_streams as usize) - num_bind_pairs;
    if num_packed_streams == 0 {
        return Err(SevenZIndexError::Malformed("folder without packed streams"));
    }
    let mut packed_streams = Vec::with_capacity(num_packed_streams);
    if num_packed_streams == 1 {
        packed_streams.push(
            (0..total_in_streams).find(|index| !in_stream_used[*index as usize]).ok_or(SevenZIndexError::Malformed("no free input stream"))?,
        );
    }
    else {
        for _ in 0..num_packed_streams {
            let index = cursor.packed_u64()?;
            if index >= total_in_streams || in_stream_used[index as usize] {
                return Err(SevenZIndexError::Malformed("packed stream references an out-of-range or used stream"));
            }
            in_stream_used[index as usize] = true;
            packed_streams.push(index);
        }
    }

    // Walk the bind chain from the packed stream to produce the coder order: each
    // coder's single output stream feeds the next coder through the bind pairs.
    let mut coder_order = Vec::with_capacity(coders.len());
    let mut current = Some(packed_streams[0]);
    let mut remaining = coders.len();
    while let Some(in_index) = current {
        if remaining == 0 {
            return Err(SevenZIndexError::Malformed("coder chain cycles"));
        }
        let coder = in_stream_owner(&coders, in_index).ok_or(SevenZIndexError::Malformed("bind pair references an unknown coder"))?;
        coder_order.push(coder);
        remaining -= 1;
        current = bind_pairs.iter().find(|(_, out)| *out == coder as u64).map(|(in_index, _)| *in_index);
    }
    if coder_order.len() != coders.len() {
        return Err(SevenZIndexError::Malformed("coder chain does not visit every coder"));
    }

    Ok(SevenZFolder {
        coders,
        coder_order,
        pack_stream_count: num_packed_streams,
        pack_ranges: Vec::new(),
        unpack_sizes: Vec::new(),
        unpack_size: 0,
        crc: None,
        num_sub_streams: 1,
    })
}

/// Finds the coder that owns the given input stream index.
fn in_stream_owner(coders: &[SevenZCoder], in_index: u64) -> Option<usize> {
    let mut base = 0u64;
    for (index, coder) in coders.iter().enumerate() {
        if in_index < base + coder.num_in_streams {
            return Some(index);
        }
        base += coder.num_in_streams;
    }
    None
}

/// Parses `SubStreamsInfo`: counts, per-sub-stream sizes, and digests.
fn parse_sub_streams_info(cursor: &mut Cursor<'_>, folders: &mut [SevenZFolder]) -> Result<SubStreams, SevenZIndexError> {
    let mut total_streams = folders.len();
    if cursor.peek() == Some(K_NUM_UNPACK_STREAM) {
        let _ = cursor.u8()?;
        total_streams = 0;
        for folder in folders.iter_mut() {
            let count = cursor.bounded_count()?;
            folder.num_sub_streams = count;
            total_streams = total_streams.checked_add(count).ok_or(SevenZIndexError::Malformed("sub-stream total overflow"))?;
            if total_streams > cursor.remaining() {
                return Err(SevenZIndexError::CountTooLarge { offset: cursor.absolute(cursor.pos), count: total_streams as u64 });
            }
        }
    }
    let mut info = SubStreams { unpack_sizes: vec![0; total_streams], has_crc: vec![false; total_streams], crcs: vec![0; total_streams] };

    let mut next = 0usize;
    let mut has_sizes = false;
    if cursor.peek() == Some(K_SIZE) {
        has_sizes = true;
        let _ = cursor.u8()?;
        for folder in folders.iter() {
            if folder.num_sub_streams == 0 {
                continue;
            }
            let mut sum = 0u64;
            for _ in 0..folder.num_sub_streams - 1 {
                let size = cursor.packed_u64()?;
                info.unpack_sizes[next] = size;
                next += 1;
                sum = sum.checked_add(size).ok_or(SevenZIndexError::Malformed("sub-stream size sum overflow"))?;
            }
            if sum > folder.unpack_size {
                return Err(SevenZIndexError::Malformed("sub-stream sizes exceed folder unpack size"));
            }
            info.unpack_sizes[next] = folder.unpack_size - sum;
            next += 1;
        }
    }

    if cursor.peek() == Some(K_CRC) {
        let _ = cursor.u8()?;
        let defined = cursor.all_or_bits(total_streams)?;
        for (index, defined) in defined.into_iter().enumerate() {
            if defined {
                info.has_crc[index] = true;
                info.crcs[index] = cursor.u32_le()?;
            }
        }
    }
    if !has_sizes {
        // Without an explicit split every folder contributes one sub-stream of its
        // total size, matching the SDK default.
        for (index, folder) in folders.iter().enumerate() {
            if folder.num_sub_streams == 1 {
                info.unpack_sizes[index] = folder.unpack_size;
            }
        }
    }
    Ok(info)
}

/// Parses `FilesInfo` and maps streamed files onto folders and sub-streams.
fn parse_files_info(
    cursor: &mut Cursor<'_>,
    folders: &mut [SevenZFolder],
    sub_streams: Option<&SubStreams>,
) -> Result<Vec<SevenZEntry>, SevenZIndexError> {
    let num_files = cursor.bounded_count()?;
    let mut entries = vec![
        SevenZEntry {
            name: String::new(),
            size: None,
            offset: None,
            attributes: None,
            is_dir: false,
            is_empty_stream: false,
            is_anti: false,
            folder_index: None,
            crc: None,
        };
        num_files
    ];
    let mut empty_stream: Option<Vec<bool>> = None;
    let mut empty_file: Option<Vec<bool>> = None;
    let mut anti: Option<Vec<bool>> = None;

    while let Some(property) = cursor.peek() {
        let _ = cursor.u8()?;
        if property == K_END {
            break;
        }
        let size = cursor.packed_u64()?;
        match property {
            K_EMPTY_STREAM => {
                let _ = size;
                empty_stream = Some(cursor.bits(num_files)?);
            }
            K_EMPTY_FILE | K_ANTI => {
                let Some(stream_bits) = empty_stream.as_ref()
                else {
                    return Err(SevenZIndexError::Malformed("empty-file or anti bitmap without empty-stream bitmap"));
                };
                let count = stream_bits.iter().filter(|defined| **defined).count();
                let bits = cursor.bits(count)?;
                if property == K_EMPTY_FILE {
                    empty_file = Some(bits);
                }
                else {
                    anti = Some(bits);
                }
            }
            K_NAME => {
                if cursor.u8()? != 0 {
                    return Err(SevenZIndexError::Malformed("external file names are not supported"));
                }
                if size == 0 {
                    return Err(SevenZIndexError::Malformed("empty name property"));
                }
                let payload_size = usize::try_from(size - 1)
                    .map_err(|_| SevenZIndexError::CountTooLarge { offset: cursor.absolute(cursor.pos), count: size })?;
                if payload_size > cursor.remaining() {
                    return Err(SevenZIndexError::CountTooLarge { offset: cursor.absolute(cursor.pos), count: payload_size as u64 });
                }
                let payload = cursor.take(payload_size)?;
                if payload.len() % 2 != 0 {
                    return Err(SevenZIndexError::Malformed("file names must be UTF-16LE"));
                }
                // Names are concatenated UTF-16LE strings separated by zero units;
                // the final name may omit its terminator.
                let mut next_file = 0usize;
                let mut units = payload.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair));
                loop {
                    let mut name_units: Vec<u16> = Vec::new();
                    let mut ended = false;
                    for unit in units.by_ref() {
                        if unit == 0 {
                            ended = true;
                            break;
                        }
                        name_units.push(unit);
                    }
                    if !ended && name_units.is_empty() {
                        break;
                    }
                    if next_file >= entries.len() {
                        return Err(SevenZIndexError::Malformed("more names than files"));
                    }
                    entries[next_file].name =
                        String::from_utf16(&name_units).map_err(|_| SevenZIndexError::Malformed("invalid UTF-16LE file name"))?;
                    next_file += 1;
                    if !ended {
                        break;
                    }
                }
                if next_file != entries.len() {
                    return Err(SevenZIndexError::Malformed("name count does not match the file count"));
                }
            }
            K_WIN_ATTRIBUTES => {
                let defined = cursor.all_or_bits(num_files)?;
                if cursor.u8()? != 0 {
                    return Err(SevenZIndexError::Malformed("external attributes are not supported"));
                }
                for (entry, defined) in entries.iter_mut().zip(defined) {
                    if defined {
                        entry.attributes = Some(cursor.u32_le()?);
                    }
                }
            }
            K_C_TIME | K_A_TIME | K_M_TIME => {
                let defined = cursor.all_or_bits(num_files)?;
                if cursor.u8()? != 0 {
                    return Err(SevenZIndexError::Malformed("external timestamps are not supported"));
                }
                for defined in defined {
                    if defined {
                        let _ = cursor.u64_le()?;
                    }
                }
            }
            K_DUMMY => {
                let size =
                    usize::try_from(size).map_err(|_| SevenZIndexError::CountTooLarge { offset: cursor.absolute(cursor.pos), count: size })?;
                let _ = cursor.take(size)?;
            }
            _ => return Err(SevenZIndexError::Malformed("unknown or unsupported file property")),
        }
    }

    // Sub-stream sizes: the declared split when present, otherwise one stream per
    // folder carrying the folder total.
    let synthesized: Vec<u64>;
    let sizes: &[u64] = match sub_streams {
        Some(info) => &info.unpack_sizes,
        None => {
            synthesized = folders.iter().map(|folder| folder.unpack_size).collect();
            &synthesized
        }
    };
    let crcs: Option<&Vec<u32>> = match sub_streams {
        Some(info) => Some(&info.crcs),
        None => None,
    };
    let has_crc: Option<&Vec<bool>> = match sub_streams {
        Some(info) => Some(&info.has_crc),
        None => None,
    };

    // Map files onto folders and sub-streams in header order.
    let mut folder_cursor = 0usize;
    let mut stream_cursor = 0usize;
    let mut remaining_in_folder = folders.first().map_or(0, |folder| folder.num_sub_streams);
    let mut empty_counter = 0usize;
    let mut offsets: Vec<u64> = Vec::with_capacity(sizes.len());
    {
        let mut folder_stream = 0usize;
        for folder in folders.iter() {
            let mut offset = 0u64;
            for _ in 0..folder.num_sub_streams {
                offsets.push(offset);
                if folder_stream < sizes.len() {
                    offset = offset.checked_add(sizes[folder_stream]).ok_or(SevenZIndexError::Malformed("sub-stream offset overflow"))?;
                }
                folder_stream += 1;
            }
        }
    }
    for (index, entry) in entries.iter_mut().enumerate() {
        let has_stream = match empty_stream.as_ref() {
            Some(bits) => !bits[index],
            None => true,
        };
        if !has_stream {
            entry.is_empty_stream = true;
            entry.is_dir = match empty_file.as_ref() {
                Some(bits) => !bits[empty_counter],
                None => true,
            };
            entry.is_anti = match anti.as_ref() {
                Some(bits) => bits[empty_counter],
                None => false,
            };
            empty_counter += 1;
            continue;
        }
        while remaining_in_folder == 0 {
            folder_cursor += 1;
            remaining_in_folder = match folders.get(folder_cursor) {
                Some(folder) => folder.num_sub_streams,
                None => 0,
            };
        }
        if folder_cursor >= folders.len() {
            return Err(SevenZIndexError::Malformed("more streamed files than folders"));
        }
        let size = sizes.get(stream_cursor).copied().ok_or(SevenZIndexError::Malformed("more streamed files than sub-streams"))?;
        entry.folder_index = Some(folder_cursor);
        entry.size = Some(size);
        entry.offset = offsets.get(stream_cursor).copied();
        entry.crc = match (has_crc, crcs) {
            (Some(bits), Some(values)) => bits.get(stream_cursor).copied().and_then(|_| values.get(stream_cursor).copied()),
            _ => None,
        };
        stream_cursor += 1;
        remaining_in_folder -= 1;
    }

    Ok(entries)
}

/// Reads exactly `len` bytes at `start`, reporting an unfetched interval as a range need.
pub(crate) fn read_range<S: ByteSource>(source: &S, start: u64, len: u64) -> Result<Vec<u8>, SevenZIndexError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let size = usize::try_from(len).map_err(|_| SevenZIndexError::HeaderOutOfBounds)?;
    let mut buffer = vec![0u8; size];
    match source.read_at(start, &mut buffer) {
        Ok(ReadOutcome::Complete) => Ok(buffer),
        Ok(ReadOutcome::Partial { bytes_read }) => {
            let missing = len - bytes_read;
            Err(SevenZIndexError::NeedRange {
                range: ByteExtent::new(start + bytes_read, missing).map_err(|_| SevenZIndexError::HeaderOutOfBounds)?,
            })
        }
        Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Unavailable { .. }) | Err(_) => {
            Err(SevenZIndexError::NeedRange { range: ByteExtent::new(start, len).map_err(|_| SevenZIndexError::HeaderOutOfBounds)? })
        }
    }
}
