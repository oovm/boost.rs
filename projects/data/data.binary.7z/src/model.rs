//! Indexed 7z container model.

use data_binary_core::ByteExtent;

/// One coder (compression method or filter) inside a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenZCoder {
    /// Decoder method id bytes as stored in the header.
    pub method: Vec<u8>,
    /// Method-specific properties (LZMA carries its five property bytes).
    pub properties: Vec<u8>,
    /// Declared input stream count.
    pub num_in_streams: u64,
    /// Declared output stream count (always one for accepted archives).
    pub num_out_streams: u64,
}

/// One packed folder: a coder chain fed by consecutive pack streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenZFolder {
    /// Coders in header order.
    pub coders: Vec<SevenZCoder>,
    /// Coders in processing order (packed stream first, filters last).
    pub coder_order: Vec<usize>,
    /// Number of pack streams feeding this folder.
    pub pack_stream_count: usize,
    /// Absolute file ranges of this folder's pack streams.
    pub pack_ranges: Vec<ByteExtent>,
    /// Unpack size of each coder's output stream, indexed by header coder order.
    pub unpack_sizes: Vec<u64>,
    /// Total unpack size of the folder output.
    pub unpack_size: u64,
    /// Folder CRC-32 when the header declares one.
    pub crc: Option<u32>,
    /// Number of file sub-streams carved out of the folder output.
    pub num_sub_streams: usize,
}

/// One archive entry (file or directory) from the files info block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenZEntry {
    /// Decoded UTF-16LE name.
    pub name: String,
    /// Unpacked size, absent for empty-stream entries.
    pub size: Option<u64>,
    /// Start of the entry's sub-stream inside its decoded folder output.
    pub offset: Option<u64>,
    /// Windows attributes when the header declares them.
    pub attributes: Option<u32>,
    /// Whether the entry is a directory (empty stream without an empty-file bit).
    pub is_dir: bool,
    /// Whether the entry has no data stream at all.
    pub is_empty_stream: bool,
    /// Whether the entry is an anti-item.
    pub is_anti: bool,
    /// Folder providing this entry's data, absent for empty-stream entries.
    pub folder_index: Option<usize>,
    /// Entry CRC-32 when the header declares one.
    pub crc: Option<u32>,
}

/// Facts indexed from a 7z container header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenZArchiveIndex {
    /// Declared container version (major, minor).
    pub version: (u8, u8),
    /// Distinct coder method ids across all folders.
    pub methods: Vec<Vec<u8>>,
    /// Packed folders in header order.
    pub folders: Vec<SevenZFolder>,
    /// Entries in header order.
    pub entries: Vec<SevenZEntry>,
    /// Sum of all folder unpack sizes.
    pub total_unpack_size: u64,
}

impl SevenZArchiveIndex {
    /// Builds the index for an empty container (zeroed start header).
    pub(crate) fn empty(version: (u8, u8)) -> Self {
        Self { version, methods: Vec::new(), folders: Vec::new(), entries: Vec::new(), total_unpack_size: 0 }
    }

    /// Number of entries in the container.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Number of packed folders in the container.
    pub fn folder_count(&self) -> usize {
        self.folders.len()
    }
}
