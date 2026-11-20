//! `data-binary-xclbin` — XCLBIN2 device image container layout, validation and authoring.
//!
//! Decodes the fixed `axlf` prefix (magic, signature fields, key block, unique id),
//! the inline 152-byte `axlf_header` and the 40-byte section header table of an
//! `xclbin2` container, bounds every section interval against the declared total
//! length, and records section kinds, names and payload slices. [`XclbinBuilder`]
//! authors well-formed containers from raw payloads with 8-byte segment alignment.
//!
//! Container structure and checks only: section payloads (PDI, AIE control packets,
//! ELF cores, metadata XML) are never interpreted, and no signature over the key
//! block is produced or verified. This is the layout gate for device-image
//! authoring — passing it says nothing about whether the emitted device code runs.
//!
//! The layout is fixed by the vendor `xclbin.h` (`axlf`, `axlf_header`,
//! `axlf_section_header`, `axlf_section_kind`) and cross-checked against a
//! factory-signed `xclbin2` image shipped inside the AMD Windows driver package.

#![warn(missing_docs)]

/// File magic of the current container revision: `"xclbin2\0"`.
pub const XCLBIN_MAGIC: &[u8; 8] = b"xclbin2\0";

/// Size of the fixed prefix: magic (8) + signature length (4) + reserved (28) +
/// key block (256) + unique id (8) + inline header (152).
pub const XCLBIN_FIXED_PREFIX_BYTES: usize = 456;

/// Size of one `axlf_section_header` entry (kind + 16-byte name + padding + offsets).
pub const XCLBIN_SECTION_HEADER_BYTES: usize = 40;

/// Upper bound on the section count accepted by the format.
pub const XCLBIN_MAX_NUM_SECTION: u32 = 0x10000;

/// Required alignment of every section payload, in bytes.
pub const XCLBIN_SEGMENT_ALIGNMENT: u64 = 8;

/// Offset of the inline `axlf_header` inside the container.
const HEADER_OFFSET: usize = 0x130;
/// Offset of the first section header entry inside the container.
const SECTIONS_OFFSET: usize = 0x1C8;

/// Known `axlf_section_kind` values. Unknown numeric kinds are preserved as-is
/// by the parser and reported through [`Section::kind`]; this enum only names
/// the kinds defined by the container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum SectionKind {
    /// FPGA bitstream.
    Bitstream = 0,
    /// Clearing bitstream.
    ClearingBitstream = 1,
    /// Embedded XML metadata.
    EmbeddedMetadata = 2,
    /// Firmware image.
    Firmware = 3,
    /// Debug data.
    DebugData = 4,
    /// Scheduler firmware.
    SchedFirmware = 5,
    /// Memory topology table.
    MemTopology = 6,
    /// Kernel-to-memory connectivity table.
    Connectivity = 7,
    /// IP layout table.
    IpLayout = 8,
    /// Debug IP layout table.
    DebugIpLayout = 9,
    /// Design check point.
    DesignCheckPoint = 10,
    /// Clock frequency topology.
    ClockFreqTopology = 11,
    /// MCS flash image.
    Mcs = 12,
    /// BMC firmware.
    Bmc = 13,
    /// Build metadata.
    BuildMetadata = 14,
    /// Key-value metadata.
    KeyValueMetadata = 15,
    /// User metadata.
    UserMetadata = 16,
    /// DNA certificate.
    DnaCertificate = 17,
    /// Programmable device image (PDI).
    Pdi = 18,
    /// Partial PDI for bitstream swap.
    BitstreamPartialPdi = 19,
    /// Partition metadata.
    PartitionMetadata = 20,
    /// Emulation data.
    EmulationData = 21,
    /// System metadata.
    SystemMetadata = 22,
    /// Soft kernel ELF.
    SoftKernel = 23,
    /// Flash ask section.
    AskFlash = 24,
    /// AIE metadata.
    AieMetadata = 25,
    /// Group topology.
    AskGroupTopology = 26,
    /// Group connectivity.
    AskGroupConnectivity = 27,
    /// SmartNIC image.
    SmartNic = 28,
    /// AIE resources.
    AieResources = 29,
    /// Overlay image.
    Overlay = 30,
    /// Vendor metadata.
    VenderMetadata = 31,
    /// AIE partition image.
    AiePartition = 32,
    /// IP metadata.
    IpMetadata = 33,
    /// AIE resources binary.
    AieResourcesBin = 34,
    /// AIE trace metadata.
    AieTraceMetadata = 35,
    /// Device tree blob overlay.
    Dtbo = 36,
}

impl SectionKind {
    /// Maps a raw section kind number onto its known name.
    pub fn from_raw(kind: u32) -> Option<Self> {
        let known = match kind {
            0 => Self::Bitstream,
            1 => Self::ClearingBitstream,
            2 => Self::EmbeddedMetadata,
            3 => Self::Firmware,
            4 => Self::DebugData,
            5 => Self::SchedFirmware,
            6 => Self::MemTopology,
            7 => Self::Connectivity,
            8 => Self::IpLayout,
            9 => Self::DebugIpLayout,
            10 => Self::DesignCheckPoint,
            11 => Self::ClockFreqTopology,
            12 => Self::Mcs,
            13 => Self::Bmc,
            14 => Self::BuildMetadata,
            15 => Self::KeyValueMetadata,
            16 => Self::UserMetadata,
            17 => Self::DnaCertificate,
            18 => Self::Pdi,
            19 => Self::BitstreamPartialPdi,
            20 => Self::PartitionMetadata,
            21 => Self::EmulationData,
            22 => Self::SystemMetadata,
            23 => Self::SoftKernel,
            24 => Self::AskFlash,
            25 => Self::AieMetadata,
            26 => Self::AskGroupTopology,
            27 => Self::AskGroupConnectivity,
            28 => Self::SmartNic,
            29 => Self::AieResources,
            30 => Self::Overlay,
            31 => Self::VenderMetadata,
            32 => Self::AiePartition,
            33 => Self::IpMetadata,
            34 => Self::AieResourcesBin,
            35 => Self::AieTraceMetadata,
            36 => Self::Dtbo,
            _ => return None,
        };
        Some(known)
    }

    /// The raw section kind number stored in the container.
    pub const fn raw(self) -> u32 {
        self as u32
    }
}

/// Structural failure while decoding or bounding an `xclbin2` container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XclbinError {
    /// The buffer is shorter than the fixed prefix.
    TooShort {
        /// Bytes available.
        have: usize,
        /// Bytes required before any header field can be read.
        need: usize,
    },
    /// The first 8 bytes are not `"xclbin2\0"`.
    BadMagic,
    /// The declared total length does not fit the buffer or the fixed prefix.
    BadLength {
        /// `m_length` as declared in the header.
        declared: u64,
        /// Actual buffer size.
        file: usize,
    },
    /// The section count exceeds [`XCLBIN_MAX_NUM_SECTION`].
    TooManySections {
        /// Declared section count.
        count: u32,
    },
    /// The section header table does not fit the declared length.
    SectionTableBeyondLength {
        /// Required table end offset.
        need: u64,
        /// Declared total length.
        declared: u64,
    },
    /// A section payload interval is not aligned to [`XCLBIN_SEGMENT_ALIGNMENT`].
    UnalignedSection {
        /// Index of the offending entry.
        index: usize,
        /// Offending offset.
        offset: u64,
    },
    /// A section payload interval overflows or exceeds the declared length.
    SectionOutOfBounds {
        /// Index of the offending entry.
        index: usize,
        /// Declared section offset.
        offset: u64,
        /// Declared section size.
        size: u64,
        /// Declared total length.
        declared: u64,
    },
    /// A section name is not valid UTF-8 up to its NUL terminator.
    InvalidSectionName {
        /// Index of the offending entry.
        index: usize,
    },
    /// A header string field is not valid UTF-8 up to its NUL terminator.
    InvalidHeaderString {
        /// Name of the offending field.
        field: &'static str,
    },
}

impl core::fmt::Display for XclbinError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort { have, need } => write!(f, "buffer too short: {have} bytes, need at least {need}"),
            Self::BadMagic => write!(f, "bad magic: expected `xclbin2\\0`"),
            Self::BadLength { declared, file } => write!(f, "declared length {declared} does not fit file size {file}"),
            Self::TooManySections { count } => write!(f, "section count {count} exceeds the format maximum {XCLBIN_MAX_NUM_SECTION}"),
            Self::SectionTableBeyondLength { need, declared } => write!(f, "section table ends at {need}, beyond declared length {declared}"),
            Self::UnalignedSection { index, offset } => {
                write!(f, "section {index} offset {offset} is not {XCLBIN_SEGMENT_ALIGNMENT}-byte aligned")
            }
            Self::SectionOutOfBounds { index, offset, size, declared } => {
                write!(f, "section {index} interval [{offset}, +{size}) exceeds declared length {declared}")
            }
            Self::InvalidSectionName { index } => write!(f, "section {index} name is not valid UTF-8"),
            Self::InvalidHeaderString { field } => write!(f, "header field `{field}` is not valid UTF-8"),
        }
    }
}

impl std::error::Error for XclbinError {}

/// One decoded section header with its borrowed payload.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    /// Raw `axlf_section_kind` number.
    pub kind: u32,
    /// Section name, NUL-trimmed (may be empty).
    pub name: &'a str,
    /// File offset of the payload.
    pub offset: u64,
    /// Payload size in bytes.
    pub size: u64,
    /// Borrowed payload bytes.
    pub data: &'a [u8],
}

impl<'a> Section<'a> {
    /// The known name of this section kind, if the container format defines it.
    pub fn known_kind(&self) -> Option<SectionKind> {
        SectionKind::from_raw(self.kind)
    }
}

/// A borrowed, validated view of an `xclbin2` container.
#[derive(Debug, Clone)]
pub struct Xclbin<'a> {
    /// `m_signature_length`; `-1` means the image carries no signature.
    pub signature_length: i32,
    /// `m_uniqueId`.
    pub unique_id: u64,
    /// `m_length`: total size of the image as declared by the header.
    pub length: u64,
    /// `m_timeStamp`: creation epoch seconds.
    pub time_stamp: u64,
    /// `m_featureRomTimeStamp`.
    pub feature_rom_time_stamp: u64,
    /// `m_versionPatch`.
    pub version_patch: u16,
    /// `m_versionMajor`.
    pub version_major: u8,
    /// `m_versionMinor`.
    pub version_minor: u8,
    /// `m_mode` (`XCLBIN_MODE`).
    pub mode: u16,
    /// `m_actionMask` (driver load hints, `AM_LOAD_AIE` / `AM_LOAD_PDI`).
    pub action_mask: u16,
    /// `m_interface_uuid`.
    pub interface_uuid: [u8; 16],
    /// `m_platformVBNV`, NUL-trimmed.
    pub platform: &'a str,
    /// The `m_next_axlf` / `uuid` union, taken as the xclbin UUID.
    pub uuid: [u8; 16],
    /// `m_debug_bin`, NUL-trimmed.
    pub debug_bin: &'a str,
    /// Decoded section table, in file order.
    pub sections: Vec<Section<'a>>,
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("bounded by validation"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("bounded by validation"))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("bounded by validation"))
}

fn cstr<'a>(bytes: &'a [u8], field: &'static str) -> Result<&'a str, XclbinError> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).map_err(|_| XclbinError::InvalidHeaderString { field })
}

impl<'a> Xclbin<'a> {
    /// Decodes and structurally validates a container.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, XclbinError> {
        if bytes.len() < XCLBIN_FIXED_PREFIX_BYTES {
            return Err(XclbinError::TooShort { have: bytes.len(), need: XCLBIN_FIXED_PREFIX_BYTES });
        }
        if &bytes[0..8] != XCLBIN_MAGIC {
            return Err(XclbinError::BadMagic);
        }
        let length = u64_at(bytes, HEADER_OFFSET);
        if length < XCLBIN_FIXED_PREFIX_BYTES as u64 || length > bytes.len() as u64 {
            return Err(XclbinError::BadLength { declared: length, file: bytes.len() });
        }
        let signature_length = u32_at(bytes, 8) as i32;
        let unique_id = u64_at(bytes, 0x128);
        let time_stamp = u64_at(bytes, HEADER_OFFSET + 0x08);
        let feature_rom_time_stamp = u64_at(bytes, HEADER_OFFSET + 0x10);
        let version_patch = u16_at(bytes, HEADER_OFFSET + 0x18);
        let version_major = bytes[HEADER_OFFSET + 0x1A];
        let version_minor = bytes[HEADER_OFFSET + 0x1B];
        let mode = u16_at(bytes, HEADER_OFFSET + 0x1C);
        let action_mask = u16_at(bytes, HEADER_OFFSET + 0x1E);
        let mut interface_uuid = [0u8; 16];
        interface_uuid.copy_from_slice(&bytes[HEADER_OFFSET + 0x20..HEADER_OFFSET + 0x30]);
        let platform = cstr(&bytes[HEADER_OFFSET + 0x30..HEADER_OFFSET + 0x70], "m_platformVBNV")?;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes[HEADER_OFFSET + 0x70..HEADER_OFFSET + 0x80]);
        let debug_bin = cstr(&bytes[HEADER_OFFSET + 0x80..HEADER_OFFSET + 0x90], "m_debug_bin")?;
        let num_sections = u32_at(bytes, HEADER_OFFSET + 0x90);
        if num_sections > XCLBIN_MAX_NUM_SECTION {
            return Err(XclbinError::TooManySections { count: num_sections });
        }
        let table_end = XCLBIN_FIXED_PREFIX_BYTES as u64 + u64::from(num_sections) * XCLBIN_SECTION_HEADER_BYTES as u64;
        if table_end > length {
            return Err(XclbinError::SectionTableBeyondLength { need: table_end, declared: length });
        }

        let mut sections = Vec::with_capacity(num_sections as usize);
        for index in 0..num_sections as usize {
            let entry = SECTIONS_OFFSET + index * XCLBIN_SECTION_HEADER_BYTES;
            let kind = u32_at(bytes, entry);
            let name = cstr(&bytes[entry + 4..entry + 20], "m_sectionName").map_err(|_| XclbinError::InvalidSectionName { index })?;
            let offset = u64_at(bytes, entry + 24);
            let size = u64_at(bytes, entry + 32);
            if offset % XCLBIN_SEGMENT_ALIGNMENT != 0 {
                return Err(XclbinError::UnalignedSection { index, offset });
            }
            let end = offset.checked_add(size).ok_or(XclbinError::SectionOutOfBounds { index, offset, size, declared: length })?;
            if end > length {
                return Err(XclbinError::SectionOutOfBounds { index, offset, size, declared: length });
            }
            let start = offset as usize;
            sections.push(Section { kind, name, offset, size, data: &bytes[start..end as usize] });
        }

        Ok(Self {
            signature_length,
            unique_id,
            length,
            time_stamp,
            feature_rom_time_stamp,
            version_patch,
            version_major,
            version_minor,
            mode,
            action_mask,
            interface_uuid,
            platform,
            uuid,
            debug_bin,
            sections,
        })
    }

    /// Whether the buffer starts with the `xclbin2` magic.
    pub fn is_xclbin(bytes: &[u8]) -> bool {
        bytes.len() >= 8 && &bytes[0..8] == XCLBIN_MAGIC
    }

    /// The first section with the given known kind.
    pub fn section(&self, kind: SectionKind) -> Option<&Section<'a>> {
        self.sections.iter().find(|section| section.kind == kind.raw())
    }

    /// All sections with the given known kind.
    pub fn sections_of(&self, kind: SectionKind) -> impl Iterator<Item = &Section<'a>> {
        self.sections.iter().filter(move |section| section.kind == kind.raw())
    }

    /// The xclbin UUID rendered as lowercase hex.
    pub fn uuid_hex(&self) -> String {
        self.uuid.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Authors a fresh `xclbin2` container from raw section payloads.
///
/// The builder writes the fixed prefix, the inline header and the section table,
/// places every payload at an 8-byte boundary and sets `m_length` to the exact
/// produced size. It produces unsigned images (`m_signature_length = -1`, key
/// block initialized to `0xFF`s like the vendor reference).
#[derive(Debug, Clone, Default)]
pub struct XclbinBuilder {
    unique_id: u64,
    time_stamp: u64,
    feature_rom_time_stamp: u64,
    version: (u16, u8, u8),
    mode: u16,
    action_mask: u16,
    interface_uuid: [u8; 16],
    platform: String,
    uuid: [u8; 16],
    debug_bin: String,
    sections: Vec<(u32, String, Vec<u8>)>,
}

impl XclbinBuilder {
    /// An empty container with version `2.1.0` and no sections.
    pub fn new() -> Self {
        Self { version: (0, 2, 1), ..Self::default() }
    }

    /// Sets `m_uniqueId`.
    pub fn unique_id(mut self, id: u64) -> Self {
        self.unique_id = id;
        self
    }

    /// Sets `m_timeStamp`.
    pub fn time_stamp(mut self, seconds: u64) -> Self {
        self.time_stamp = seconds;
        self
    }

    /// Sets `m_featureRomTimeStamp`.
    pub fn feature_rom_time_stamp(mut self, seconds: u64) -> Self {
        self.feature_rom_time_stamp = seconds;
        self
    }

    /// Sets the header version triple.
    pub fn version(mut self, patch: u16, major: u8, minor: u8) -> Self {
        self.version = (patch, major, minor);
        self
    }

    /// Sets `m_mode` (`XCLBIN_MODE`).
    pub fn mode(mut self, mode: u16) -> Self {
        self.mode = mode;
        self
    }

    /// Sets `m_actionMask`.
    pub fn action_mask(mut self, mask: u16) -> Self {
        self.action_mask = mask;
        self
    }

    /// Sets `m_interface_uuid`.
    pub fn interface_uuid(mut self, uuid: [u8; 16]) -> Self {
        self.interface_uuid = uuid;
        self
    }

    /// Sets `m_platformVBNV` (truncated to the field's NUL-terminated capacity).
    pub fn platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = platform.into();
        self
    }

    /// Sets the xclbin UUID.
    pub fn uuid(mut self, uuid: [u8; 16]) -> Self {
        self.uuid = uuid;
        self
    }

    /// Sets `m_debug_bin`.
    pub fn debug_bin(mut self, name: impl Into<String>) -> Self {
        self.debug_bin = name.into();
        self
    }

    /// Appends one section payload.
    pub fn add_section(mut self, kind: SectionKind, name: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        self.sections.push((kind.raw(), name.into(), payload.into()));
        self
    }

    /// Appends one section with a raw (possibly vendor-extended) kind number.
    pub fn add_raw_section(mut self, kind: u32, name: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        self.sections.push((kind, name.into(), payload.into()));
        self
    }

    /// Serializes the container.
    pub fn build(&self) -> Vec<u8> {
        let table_bytes = XCLBIN_FIXED_PREFIX_BYTES + self.sections.len() * XCLBIN_SECTION_HEADER_BYTES;
        let mut payload_total = 0usize;
        let mut offsets = Vec::with_capacity(self.sections.len());
        for (_, _, payload) in &self.sections {
            let start = align_up(table_bytes + payload_total, XCLBIN_SEGMENT_ALIGNMENT as usize);
            offsets.push(start);
            payload_total = start - table_bytes + payload.len();
        }
        let total = align_up(table_bytes + payload_total, XCLBIN_SEGMENT_ALIGNMENT as usize);

        let mut bytes = vec![0u8; total];
        bytes[0..8].copy_from_slice(XCLBIN_MAGIC);
        bytes[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        bytes[12..40].fill(0xFF);
        bytes[40..296].fill(0xFF);
        bytes[0x128..0x130].copy_from_slice(&self.unique_id.to_le_bytes());

        let h = HEADER_OFFSET;
        bytes[h..h + 8].copy_from_slice(&(total as u64).to_le_bytes());
        bytes[h + 8..h + 16].copy_from_slice(&self.time_stamp.to_le_bytes());
        bytes[h + 16..h + 24].copy_from_slice(&self.feature_rom_time_stamp.to_le_bytes());
        bytes[h + 24..h + 26].copy_from_slice(&self.version.0.to_le_bytes());
        bytes[h + 26] = self.version.1;
        bytes[h + 27] = self.version.2;
        bytes[h + 28..h + 30].copy_from_slice(&self.mode.to_le_bytes());
        bytes[h + 30..h + 32].copy_from_slice(&self.action_mask.to_le_bytes());
        bytes[h + 32..h + 48].copy_from_slice(&self.interface_uuid);
        write_field(&mut bytes[h + 48..h + 112], self.platform.as_bytes());
        bytes[h + 112..h + 128].copy_from_slice(&self.uuid);
        write_field(&mut bytes[h + 128..h + 144], self.debug_bin.as_bytes());
        bytes[h + 144..h + 148].copy_from_slice(&(self.sections.len() as u32).to_le_bytes());

        for (index, (kind, name, payload)) in self.sections.iter().enumerate() {
            let entry = SECTIONS_OFFSET + index * XCLBIN_SECTION_HEADER_BYTES;
            bytes[entry..entry + 4].copy_from_slice(&kind.to_le_bytes());
            write_field(&mut bytes[entry + 4..entry + 20], name.as_bytes());
            let offset = offsets[index] as u64;
            bytes[entry + 24..entry + 32].copy_from_slice(&offset.to_le_bytes());
            bytes[entry + 32..entry + 40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
            let start = offsets[index];
            bytes[start..start + payload.len()].copy_from_slice(payload);
        }
        bytes
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) / alignment * alignment
}

fn write_field(field: &mut [u8], value: &[u8]) {
    let take = value.len().min(field.len() - 1);
    field[..take].copy_from_slice(&value[..take]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        XclbinBuilder::new()
            .unique_id(7)
            .time_stamp(1_600_000_000)
            .platform("xilinx_v1_ipu_0_0")
            .uuid([0xAB; 16])
            .action_mask(0x3)
            .add_section(SectionKind::MemTopology, "mem_topology", vec![1u8; 0x58])
            .add_section(SectionKind::AieMetadata, "aie_metadata", vec![2u8; 0x333])
            .add_section(SectionKind::EmbeddedMetadata, "emb", b"<?xml version=\"1.0\"?>".to_vec())
            .build()
    }

    #[test]
    fn round_trips_header_fields_and_section_payloads() {
        let bytes = sample();
        let image = Xclbin::parse(&bytes).expect("valid");
        assert_eq!(image.length, bytes.len() as u64);
        assert_eq!(image.signature_length, -1);
        assert_eq!(image.unique_id, 7);
        assert_eq!(image.time_stamp, 1_600_000_000);
        assert_eq!(image.platform, "xilinx_v1_ipu_0_0");
        assert_eq!(image.uuid, [0xAB; 16]);
        assert_eq!(image.action_mask, 0x3);
        assert_eq!(image.sections.len(), 3);
        assert_eq!(image.sections[0].known_kind(), Some(SectionKind::MemTopology));
        assert_eq!(image.sections[1].name, "aie_metadata");
        assert_eq!(image.sections[1].data, &[2u8; 0x333]);
        assert_eq!(image.section(SectionKind::EmbeddedMetadata).expect("xml").data, b"<?xml version=\"1.0\"?>");
        assert_eq!(image.sections_of(SectionKind::MemTopology).count(), 1);
        assert_eq!(image.uuid_hex().len(), 32);
    }

    #[test]
    fn every_section_payload_is_aligned() {
        let bytes = sample();
        let image = Xclbin::parse(&bytes).expect("valid");
        for section in &image.sections {
            assert_eq!(section.offset % XCLBIN_SEGMENT_ALIGNMENT, 0, "{}", section.name);
            assert!((section.offset + section.size) as usize <= bytes.len());
        }
    }

    #[test]
    fn empty_container_is_valid() {
        let bytes = XclbinBuilder::new().build();
        let image = Xclbin::parse(&bytes).expect("valid");
        assert!(image.sections.is_empty());
        assert_eq!(image.length, XCLBIN_FIXED_PREFIX_BYTES as u64);
    }

    #[test]
    fn rejects_bad_magic_and_truncation() {
        assert_eq!(Xclbin::parse(&[0u8; 64]).unwrap_err(), XclbinError::TooShort { have: 64, need: XCLBIN_FIXED_PREFIX_BYTES });
        let mut bytes = sample();
        bytes[1] = b'X';
        assert_eq!(Xclbin::parse(&bytes).unwrap_err(), XclbinError::BadMagic);
        let bytes = &sample()[..XCLBIN_FIXED_PREFIX_BYTES - 1];
        assert!(matches!(Xclbin::parse(bytes), Err(XclbinError::TooShort { .. })));
    }

    #[test]
    fn rejects_out_of_bounds_unaligned_and_overflowing_sections() {
        let mut bytes = sample();
        let entry = SECTIONS_OFFSET;
        // Payload end beyond declared length.
        bytes[entry + 32..entry + 40].copy_from_slice(&0xFFFFu64.to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::SectionOutOfBounds { index: 0, .. })));

        // Unaligned offset.
        let mut bytes = sample();
        bytes[entry + 24..entry + 32].copy_from_slice(&0x381u64.to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::UnalignedSection { index: 0, offset: 0x381 })));

        // Offset + size overflow (aligned near-max offset so the bound check runs).
        let mut bytes = sample();
        bytes[entry + 24..entry + 32].copy_from_slice(&(u64::MAX - 7).to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::SectionOutOfBounds { .. })));
    }

    #[test]
    fn rejects_inconsistent_length_and_section_count() {
        let mut bytes = sample();
        let declared = bytes.len() as u64 + 1;
        bytes[HEADER_OFFSET..HEADER_OFFSET + 8].copy_from_slice(&declared.to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::BadLength { .. })));

        let mut bytes = sample();
        bytes[HEADER_OFFSET + 0x90..HEADER_OFFSET + 0x94].copy_from_slice(&0x20000u32.to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::TooManySections { count: 0x20000 })));

        let mut bytes = sample();
        bytes[HEADER_OFFSET + 0x90..HEADER_OFFSET + 0x94].copy_from_slice(&100u32.to_le_bytes());
        assert!(matches!(Xclbin::parse(&bytes), Err(XclbinError::SectionTableBeyondLength { .. })));
    }

    #[test]
    fn magic_probe_matches_built_containers() {
        assert!(Xclbin::is_xclbin(&sample()));
        assert!(!Xclbin::is_xclbin(b"not an xclbin image"));
    }

    #[test]
    fn long_names_are_truncated_to_the_field_capacity() {
        let bytes = XclbinBuilder::new().add_section(SectionKind::SoftKernel, "a_kernel_name_that_is_way_too_long", vec![9u8; 8]).build();
        let image = Xclbin::parse(&bytes).expect("valid");
        assert_eq!(image.sections[0].name.len(), 15);
        assert_eq!(image.sections[0].data, &[9u8; 8]);
    }
}
