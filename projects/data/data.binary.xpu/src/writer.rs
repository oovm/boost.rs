//! Deterministic writer: directory, aligned section placement, digests and
//! the fixed header (contract 42 §2).
//!
//! Section ids are assigned `1..=n` in input order. The layout is fully
//! determined by the input, so building the same spec twice produces the
//! same bytes.

use crate::error::XpuError;
use crate::index::{
    ENTRY_BYTES, KNOWN_SECTION_FLAGS, MAX_CONTROL_BYTES, MAX_SECTIONS, MIN_ALIGNMENT, Section, SectionKind, sha256_bytes,
    write_entry,
};
use crate::shell::{self, FORMAT_MAJOR, FORMAT_MINOR, HEADER_BYTES, Header};

/// One section to place in the container.
#[derive(Debug, Clone)]
pub struct SectionInput {
    /// Raw kind value; `SectionKind::as_raw` produces the registered ones.
    pub kind: u16,
    pub schema_rev: u16,
    /// Section flags; only bit 0 (globally required) is defined in v1.0.
    pub flags: u32,
    /// 0 selects the kind default (64 B for weights, 8 B otherwise).
    pub alignment: u32,
    pub data: Vec<u8>,
}

/// Everything needed to build one container deterministically.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub sections: Vec<SectionInput>,
}

fn align_up(x: u64, a: u32) -> u64 {
    let a = a as u64;
    x.div_ceil(a) * a
}

/// Build the container bytes.
pub fn build(spec: &ContainerSpec) -> Result<Vec<u8>, XpuError> {
    let n = spec.sections.len();
    if n == 0 || n > MAX_SECTIONS as usize {
        return Err(XpuError::WriterInput(format!("section count must be 1..={MAX_SECTIONS}, got {n}")));
    }

    // Validate inputs and resolve alignments before laying anything out.
    let mut control: u64 = 0;
    for (i, s) in spec.sections.iter().enumerate() {
        let id = (i + 1) as u32;
        if s.flags & !KNOWN_SECTION_FLAGS != 0 {
            return Err(XpuError::WriterInput(format!("section {id} uses reserved flag bits")));
        }
        let kind = SectionKind::from_raw(s.kind);
        if matches!(kind, SectionKind::Unknown(_)) && s.flags & crate::index::FLAG_GLOBAL_REQUIRED != 0 {
            return Err(XpuError::WriterInput(format!("section {id} has unknown kind {} and is globally required", s.kind)));
        }
        if s.alignment != 0 && (s.alignment < MIN_ALIGNMENT || !s.alignment.is_power_of_two()) {
            return Err(XpuError::WriterInput(format!("section {id} alignment must be 0 or a power of two of at least 8")));
        }
        if kind.is_control() {
            control = control.checked_add(s.data.len() as u64).ok_or(XpuError::ControlBudgetExceeded {
                declared: u64::MAX,
                limit: MAX_CONTROL_BYTES,
            })?;
        }
    }
    if control > MAX_CONTROL_BYTES {
        return Err(XpuError::ControlBudgetExceeded { declared: control, limit: MAX_CONTROL_BYTES });
    }
    for singleton in [SectionKind::Manifest, SectionKind::Graph, SectionKind::TensorIndex] {
        let hits = spec.sections.iter().filter(|s| s.kind == singleton.as_raw()).count();
        if hits != 1 {
            return Err(XpuError::WriterInput(format!(
                "kind {} must appear exactly once, got {hits}",
                singleton.as_raw()
            )));
        }
    }
    let manifest_id = spec.sections.iter().position(|s| s.kind == SectionKind::Manifest.as_raw()).expect("validated") as u32 + 1;

    // Place sections after the directory.
    let directory_end = (HEADER_BYTES + n * ENTRY_BYTES) as u64;
    let mut cursor = directory_end;
    let mut entries = Vec::with_capacity(n);
    for (i, s) in spec.sections.iter().enumerate() {
        let kind = SectionKind::from_raw(s.kind);
        let alignment = if s.alignment == 0 { kind.default_alignment() } else { s.alignment };
        let offset = align_up(cursor, alignment);
        cursor = offset + s.data.len() as u64;
        entries.push(Section {
            id: (i + 1) as u32,
            kind: s.kind,
            schema_rev: s.schema_rev,
            flags: s.flags,
            alignment,
            offset,
            stored_bytes: s.data.len() as u64,
            logical_bytes: s.data.len() as u64,
            sha256: sha256_bytes(&s.data),
        });
    }

    // Directory bytes, then header, then the payload image.
    let mut directory = Vec::with_capacity(n * ENTRY_BYTES);
    for e in &entries {
        write_entry(e, &mut directory);
    }
    let header = Header {
        major: FORMAT_MAJOR,
        minor: FORMAT_MINOR,
        required_flags: 0,
        section_count: n as u32,
        directory_offset: HEADER_BYTES as u64,
        file_bytes: cursor,
        manifest_id,
        directory_sha256: sha256_bytes(&directory),
    };

    let mut out = shell::write(&header);
    out.extend_from_slice(&directory);
    out.resize(cursor as usize, 0);
    for (e, s) in entries.iter().zip(&spec.sections) {
        let at = e.offset as usize;
        out[at..at + s.data.len()].copy_from_slice(&s.data);
    }
    Ok(out)
}
