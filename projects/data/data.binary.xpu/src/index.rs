//! Section kinds, flags, limits and the 80-byte directory entry
//! (contract 42 §2 and §2.1).

use sha2::{Digest, Sha256};

/// Maximum number of directory entries (§2: 65,536).
pub const MAX_SECTIONS: u32 = 65_536;

/// Size of one fixed directory entry.
pub const ENTRY_BYTES: usize = 80;

/// Budget for the control area: every section except weights and device
/// module images (§2). Deployment may tighten it, never widen it.
pub const MAX_CONTROL_BYTES: u64 = 64 * 1024 * 1024;

/// Smallest alignment a section may declare.
pub const MIN_ALIGNMENT: u32 = 8;

/// Default alignment the writer uses for weight sections (§2: at least 64 B).
pub const WEIGHT_ALIGNMENT: u32 = 64;

/// Default alignment for every other section.
pub const DEFAULT_ALIGNMENT: u32 = 8;

/// Section flag bit 0: the section is globally required.
pub const FLAG_GLOBAL_REQUIRED: u32 = 1;

/// Section flag bits defined by v1.0.
pub const KNOWN_SECTION_FLAGS: u32 = FLAG_GLOBAL_REQUIRED;

/// Header `required_flags` bits defined by v1.0 (none).
pub const KNOWN_REQUIRED_FLAGS: u32 = 0;

/// SHA-256 over raw bytes, the digest primitive of the container.
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Section kinds registered by contract 42 §2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// Required singleton: model identity and section references.
    Manifest,
    /// Required singleton: portable graph.
    Graph,
    /// Required singleton: tensor ranges over weight sections.
    TensorIndex,
    /// Zero or more: read-only tensor data.
    Weight,
    /// Optional: device variant targets and coverage.
    VariantIndex,
    /// Optional: execution plan per variant.
    Plan,
    /// Optional: native loadable module image.
    Module,
    /// Optional: instruction streams and auxiliary data.
    Aux,
    /// Reserved for release attestation; v1.0 defines no payload encoding.
    Signature,
    /// A kind this format revision does not know.
    Unknown(u16),
}

impl SectionKind {
    pub fn from_raw(raw: u16) -> Self {
        match raw {
            1 => SectionKind::Manifest,
            2 => SectionKind::Graph,
            3 => SectionKind::TensorIndex,
            4 => SectionKind::Weight,
            5 => SectionKind::VariantIndex,
            6 => SectionKind::Plan,
            7 => SectionKind::Module,
            8 => SectionKind::Aux,
            9 => SectionKind::Signature,
            other => SectionKind::Unknown(other),
        }
    }

    pub fn as_raw(self) -> u16 {
        match self {
            SectionKind::Manifest => 1,
            SectionKind::Graph => 2,
            SectionKind::TensorIndex => 3,
            SectionKind::Weight => 4,
            SectionKind::VariantIndex => 5,
            SectionKind::Plan => 6,
            SectionKind::Module => 7,
            SectionKind::Aux => 8,
            SectionKind::Signature => 9,
            SectionKind::Unknown(raw) => raw,
        }
    }

    /// Kinds that must appear exactly once in every container.
    pub fn is_singleton(self) -> bool {
        matches!(self, SectionKind::Manifest | SectionKind::Graph | SectionKind::TensorIndex)
    }

    /// Whether this kind counts against the control-area budget. Weights and
    /// device module images are data, not control.
    pub fn is_control(self) -> bool {
        !matches!(self, SectionKind::Weight | SectionKind::Module)
    }

    /// Alignment the writer uses when the caller does not request one.
    pub fn default_alignment(self) -> u32 {
        match self {
            SectionKind::Weight => WEIGHT_ALIGNMENT,
            _ => DEFAULT_ALIGNMENT,
        }
    }
}

/// One parsed directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub id: u32,
    pub kind: u16,
    pub schema_rev: u16,
    pub flags: u32,
    pub alignment: u32,
    pub offset: u64,
    pub stored_bytes: u64,
    pub logical_bytes: u64,
    pub sha256: [u8; 32],
}

impl Section {
    pub fn kind_parsed(&self) -> SectionKind {
        SectionKind::from_raw(self.kind)
    }

    pub fn is_globally_required(&self) -> bool {
        self.flags & FLAG_GLOBAL_REQUIRED != 0
    }
}

/// Read one 80-byte entry. Structural checks happen in the reader, which
/// has the file context the entry alone cannot see.
pub(crate) fn parse_entry(b: &[u8]) -> Section {
    Section {
        id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        kind: u16::from_le_bytes([b[4], b[5]]),
        schema_rev: u16::from_le_bytes([b[6], b[7]]),
        flags: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        alignment: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        offset: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        stored_bytes: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        logical_bytes: u64::from_le_bytes(b[32..40].try_into().unwrap()),
        sha256: b[40..72].try_into().unwrap(),
    }
}

/// Serialise one entry; the 8 reserved trailing bytes stay zero.
pub(crate) fn write_entry(s: &Section, out: &mut Vec<u8>) {
    out.extend_from_slice(&s.id.to_le_bytes());
    out.extend_from_slice(&s.kind.to_le_bytes());
    out.extend_from_slice(&s.schema_rev.to_le_bytes());
    out.extend_from_slice(&s.flags.to_le_bytes());
    out.extend_from_slice(&s.alignment.to_le_bytes());
    out.extend_from_slice(&s.offset.to_le_bytes());
    out.extend_from_slice(&s.stored_bytes.to_le_bytes());
    out.extend_from_slice(&s.logical_bytes.to_le_bytes());
    out.extend_from_slice(&s.sha256);
    out.extend_from_slice(&[0u8; 8]);
}
