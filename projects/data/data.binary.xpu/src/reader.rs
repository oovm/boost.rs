//! Structural validation and bounded section access (contract 42 §2).
//!
//! `Container::parse` performs every check that can be made from the header
//! and the directory alone, in contract order: limits before allocation,
//! ranges before use. Section digests are verified on demand — control
//! sections before use, weight shards before they enter a working set — so
//! opening a file never forces a full scan of unreferenced data.

use crate::error::XpuError;
use crate::index::{
    ENTRY_BYTES, KNOWN_SECTION_FLAGS, MAX_CONTROL_BYTES, MAX_SECTIONS, MIN_ALIGNMENT, Section, SectionKind, parse_entry,
    sha256_bytes,
};
use crate::shell::{self, HEADER_BYTES, Header};

/// A validated `.xpu` container viewed over its raw bytes.
#[derive(Debug, Clone)]
pub struct Container<'a> {
    bytes: &'a [u8],
    header: Header,
    sections: Vec<Section>,
    directory_end: u64,
}

impl<'a> Container<'a> {
    /// Parse the shell and run every structural validation the contract
    /// places before payload access.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, XpuError> {
        let actual = bytes.len() as u64;
        let header = shell::parse(bytes, actual)?;

        // Limit the entry count before touching the directory.
        let count = header.section_count;
        if count == 0 || count > MAX_SECTIONS {
            return Err(XpuError::TooManySections(count));
        }
        let dir_len = (count as u64)
            .checked_mul(ENTRY_BYTES as u64)
            .ok_or(XpuError::TooManySections(count))?;
        let directory_end =
            header.directory_offset.checked_add(dir_len).ok_or(XpuError::TooManySections(count))?;
        if directory_end > actual {
            return Err(XpuError::TooShort { need: directory_end, have: actual });
        }
        let directory = &bytes[header.directory_offset as usize..directory_end as usize];
        if sha256_bytes(directory) != header.directory_sha256 {
            return Err(XpuError::DirectoryDigestMismatch);
        }

        let mut sections = Vec::with_capacity(count as usize);
        let mut prev_id = 0u32;
        for raw in directory.chunks_exact(ENTRY_BYTES) {
            let s = parse_entry(raw);
            if s.id == 0 || s.id <= prev_id {
                return Err(XpuError::BadOrder);
            }
            prev_id = s.id;
            if s.flags & !KNOWN_SECTION_FLAGS != 0 {
                return Err(XpuError::ReservedSectionFlags(s.flags));
            }
            if s.alignment < MIN_ALIGNMENT || !s.alignment.is_power_of_two() {
                return Err(XpuError::BadAlignment(s.id));
            }
            if s.stored_bytes != s.logical_bytes {
                return Err(XpuError::Compressed(s.id));
            }
            sections.push(s);
        }

        // Control budget over declared sizes, before trusting any range.
        let mut control: u64 = 0;
        for s in &sections {
            if SectionKind::from_raw(s.kind).is_control() {
                control = control.checked_add(s.stored_bytes).unwrap_or(u64::MAX);
            }
        }
        if control > MAX_CONTROL_BYTES {
            return Err(XpuError::ControlBudgetExceeded { declared: control, limit: MAX_CONTROL_BYTES });
        }

        // Ranges: fully inside the file, after the directory, aligned.
        for s in &sections {
            let end = match s.offset.checked_add(s.stored_bytes) {
                Some(e) => e,
                None => return Err(XpuError::RangeViolation { id: s.id, offset: s.offset, end: u64::MAX }),
            };
            if s.offset < directory_end || end > header.file_bytes {
                return Err(XpuError::RangeViolation { id: s.id, offset: s.offset, end });
            }
            if s.stored_bytes > 0 && s.offset % s.alignment as u64 != 0 {
                return Err(XpuError::Misaligned { id: s.id, offset: s.offset, alignment: s.alignment as u64 });
            }
        }

        // Non-empty sections must not overlap.
        let mut spans: Vec<(u64, u64, u32)> =
            sections.iter().filter(|s| s.stored_bytes > 0).map(|s| (s.offset, s.offset + s.stored_bytes, s.id)).collect();
        spans.sort_unstable();
        for w in spans.windows(2) {
            if w[1].0 < w[0].1 {
                return Err(XpuError::Overlap { a: w[0].2, b: w[1].2 });
            }
        }

        // Kind-level rules: required singletons, unknown required sections,
        // and the manifest reference.
        let mut manifest: Option<&Section> = None;
        for kind_raw in [SectionKind::Manifest, SectionKind::Graph, SectionKind::TensorIndex] {
            let hits: Vec<&Section> = sections.iter().filter(|s| s.kind == kind_raw.as_raw()).collect();
            match hits.len() {
                0 => return Err(XpuError::MissingSingleton(kind_raw.as_raw())),
                1 => {
                    if kind_raw == SectionKind::Manifest {
                        manifest = Some(hits[0]);
                    }
                }
                n => return Err(XpuError::DuplicateSingleton { kind: kind_raw.as_raw(), count: n as u32 }),
            }
        }
        for s in &sections {
            if matches!(SectionKind::from_raw(s.kind), SectionKind::Unknown(_)) && s.is_globally_required() {
                return Err(XpuError::RequiredUnknownKind(s.id));
            }
        }
        let manifest = manifest.expect("singleton check guarantees one manifest");
        if header.manifest_id != manifest.id {
            return Err(XpuError::ManifestRefMismatch(header.manifest_id));
        }

        Ok(Container { bytes, header, sections, directory_end })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The parsed directory, sorted by id.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn section(&self, id: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }

    /// The manifest section (always present after `parse`).
    pub fn manifest(&self) -> &Section {
        self.sections.iter().find(|s| s.kind == SectionKind::Manifest.as_raw()).expect("validated singleton")
    }

    pub fn kind_sections(&self, kind: SectionKind) -> impl Iterator<Item = &Section> {
        let raw = kind.as_raw();
        self.sections.iter().filter(move |s| s.kind == raw)
    }

    /// Raw stored bytes of a section. Ranges were validated at parse time.
    pub fn read(&self, id: u32) -> Option<&'a [u8]> {
        let s = self.section(id)?;
        Some(&self.bytes[s.offset as usize..(s.offset + s.stored_bytes) as usize])
    }

    /// Verify one section digest over its stored bytes.
    pub fn verify(&self, id: u32) -> Result<(), XpuError> {
        let s = self.section(id).ok_or(XpuError::UnknownSection(id))?;
        let data = &self.bytes[s.offset as usize..(s.offset + s.stored_bytes) as usize];
        if sha256_bytes(data) == s.sha256 {
            Ok(())
        } else {
            Err(XpuError::SectionDigestMismatch(id))
        }
    }

    /// Verify every section digest (full audit path).
    pub fn verify_all(&self) -> Result<(), XpuError> {
        for s in &self.sections {
            self.verify(s.id)?;
        }
        Ok(())
    }
}
