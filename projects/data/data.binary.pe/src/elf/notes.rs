//! `ELF` 注释（note）解码：`PT_NOTE` 段优先，缺失时回退到 `SHT_NOTE` 节。
//!
//! 覆盖 GNU build id、ABI tag 与 GNU property 条目。畸形记录会截断列表并置
//! `truncated`，而不是让整份报告失败。

use super::read::{Cursor, ElfParseError, parse_elf, parse_elf_header};
use super::segments::{ElfSegmentType, read_program_headers};

const SHT_NOTE: u32 = 7;

const NT_GNU_ABI_TAG: u64 = 1;
const NT_GNU_BUILD_ID: u64 = 3;
const NT_GNU_GOLD_VERSION: u64 = 4;
const NT_GNU_PROPERTY_TYPE_0: u64 = 5;

const NOTE_HEADER_SIZE: u64 = 12;
const MAX_REGIONS: usize = 64;
const MAX_NOTES: usize = 128;
const MAX_PROPERTIES: usize = 32;
const MAX_DESCRIPTION_PREVIEW: usize = 32;
const MAX_BUILD_ID: usize = 64;

/// 解码后的 `NT_GNU_ABI_TAG`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfAbiTag {
    /// 目标操作系统名。
    pub os: String,
    /// 主版本号。
    pub major: u32,
    /// 次版本号。
    pub minor: u32,
    /// 修订号。
    pub subminor: u32,
}

/// `NT_GNU_PROPERTY_TYPE_0` 中的一条属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfGnuProperty {
    /// 稳定属性名，未知值渲染为 `0x<hex>`。
    pub property_type: String,
    /// 本属性声明的大小。
    pub data_size: u32,
    /// 属性数据的小写十六进制，受预览上限约束。
    pub data: String,
}

/// 一条 note 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfNote {
    /// note 所有者（去掉 NUL），例如 `GNU`。
    pub owner: String,
    /// 稳定类型名，未知值渲染为 `type <n>`。
    pub note_type: String,
    /// 声明的描述大小。
    pub description_size: u32,
    /// 解码后的描述：build id 十六进制、ABI 版本、属性数量或有界十六进制预览。
    pub description: String,
}

/// note 的读取来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfNoteSource {
    /// 来自 `PT_NOTE` 程序头。
    Segments,
    /// 来自 `SHT_NOTE` 节。
    Sections,
}

impl ElfNoteSource {
    /// 报告使用的稳定小写名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Sections => "sections",
        }
    }
}

/// 一张 `ELF` 镜像的 note 事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfNotes {
    /// 是否至少存在一个 note 区域。
    pub present: bool,
    /// note 的读取来源。
    pub source: Option<ElfNoteSource>,
    /// `NT_GNU_BUILD_ID` 的小写十六进制。
    pub build_id: Option<String>,
    /// 解码后的 `NT_GNU_ABI_TAG`。
    pub abi_tag: Option<ElfAbiTag>,
    /// 第一条 `NT_GNU_PROPERTY_TYPE_0` 的属性条目。
    pub properties: Vec<ElfGnuProperty>,
    /// 记录，按文件顺序。
    pub notes: Vec<ElfNote>,
    /// 应用上限前读取的记录数量。
    pub total: usize,
    /// 是否因上限或畸形记录截断。
    pub truncated: bool,
}

impl ElfNotes {
    fn empty() -> Self {
        Self {
            present: false,
            source: None,
            build_id: None,
            abi_tag: None,
            properties: Vec::new(),
            notes: Vec::new(),
            total: 0,
            truncated: false,
        }
    }
}

/// 读取 `ELF32` / `ELF64` 的 note 事实。
pub fn read_notes(bytes: &[u8]) -> Result<ElfNotes, ElfParseError> {
    let header = parse_elf_header(bytes)?;
    let cursor = Cursor::new(bytes, header.endian);

    let mut regions = note_segments(bytes)?;
    let mut source = ElfNoteSource::Segments;
    if regions.is_empty() {
        regions = note_sections(bytes)?;
        source = ElfNoteSource::Sections;
    }
    if regions.is_empty() {
        return Ok(ElfNotes::empty());
    }

    let mut notes = ElfNotes { present: true, source: Some(source), ..ElfNotes::empty() };
    for (offset, size) in regions.into_iter().take(MAX_REGIONS) {
        parse_region(&cursor, offset, size, &mut notes);
    }
    Ok(notes)
}

fn note_segments(bytes: &[u8]) -> Result<Vec<(u64, u64)>, ElfParseError> {
    let program = read_program_headers(bytes)?;
    Ok(program
        .segments
        .iter()
        .filter(|segment| segment.segment_type == ElfSegmentType::Note && segment.file_size > 0)
        .map(|segment| (segment.offset, segment.file_size))
        .collect())
}

fn note_sections(bytes: &[u8]) -> Result<Vec<(u64, u64)>, ElfParseError> {
    let image = parse_elf(bytes)?;
    Ok(image
        .sections
        .iter()
        .filter(|section| section.section_type == SHT_NOTE && section.size > 0)
        .map(|section| (section.offset, section.size))
        .collect())
}

fn parse_region(cursor: &Cursor<'_>, offset: u64, size: u64, notes: &mut ElfNotes) {
    let end = offset.saturating_add(size);
    let mut at = offset;
    while at.saturating_add(NOTE_HEADER_SIZE) <= end {
        let (Ok(namesz), Ok(descsz), Ok(kind)) = (cursor.u32(at), cursor.u32(at + 4), cursor.u32(at + 8))
        else {
            notes.truncated = true;
            return;
        };
        if namesz == 0 && descsz == 0 && kind == 0 {
            // 最后一条 note 之后的零填充不是 note。
            break;
        }
        let name_start = at + NOTE_HEADER_SIZE;
        let name_end = name_start.saturating_add(u64::from(namesz));
        let desc_start = align_u64(name_end, 4);
        let desc_end = desc_start.saturating_add(u64::from(descsz));
        let next = align_u64(desc_end, 4);
        if desc_end > end || next > end {
            notes.truncated = true;
            return;
        }
        let (Ok(name), Ok(description)) =
            (cursor.slice(name_start, u64::from(namesz)), cursor.slice(desc_start, u64::from(descsz)))
        else {
            notes.truncated = true;
            return;
        };

        notes.total += 1;
        let owner = String::from_utf8_lossy(name).trim_end_matches('\u{0}').to_string();
        let note_type = note_type_name(&owner, kind as u64);
        let rendered = describe(&owner, kind as u64, description, notes);

        if notes.notes.len() < MAX_NOTES {
            notes.notes.push(ElfNote { owner, note_type, description_size: descsz, description: rendered });
        }
        else {
            notes.truncated = true;
        }
        at = next;
    }
    if at < end {
        // 尾部不足一条 note 头时，全零视为填充，否则视为畸形。
        let padding = cursor.slice(at, end - at).is_ok_and(|tail| tail.iter().all(|byte| *byte == 0));
        if !padding {
            notes.truncated = true;
        }
    }
}

fn describe(owner: &str, kind: u64, description: &[u8], notes: &mut ElfNotes) -> String {
    if owner != "GNU" {
        return hex_preview(description);
    }
    match kind {
        NT_GNU_BUILD_ID => {
            let build_id = hex_preview_bounded(description, MAX_BUILD_ID);
            if notes.build_id.is_none() {
                notes.build_id = Some(build_id.clone());
            }
            build_id
        }
        NT_GNU_ABI_TAG => {
            if description.len() < 16 {
                return hex_preview(description);
            }
            let os = u32::from_le_bytes([description[0], description[1], description[2], description[3]]);
            let major = u32::from_le_bytes([description[4], description[5], description[6], description[7]]);
            let minor = u32::from_le_bytes([description[8], description[9], description[10], description[11]]);
            let subminor = u32::from_le_bytes([description[12], description[13], description[14], description[15]]);
            let tag = ElfAbiTag { os: abi_os_name(os), major, minor, subminor };
            let rendered = format!("{} {}.{}.{}", tag.os, tag.major, tag.minor, tag.subminor);
            let _ = notes.abi_tag.insert(tag);
            rendered
        }
        NT_GNU_PROPERTY_TYPE_0 => {
            let count = read_properties(description, notes);
            format!("{count} property entr{}", if count == 1 { "y" } else { "ies" })
        }
        NT_GNU_GOLD_VERSION => {
            let end = description.iter().position(|byte| *byte == 0).unwrap_or(description.len());
            String::from_utf8_lossy(&description[..end]).into_owned()
        }
        _ => hex_preview(description),
    }
}

fn read_properties(data: &[u8], notes: &mut ElfNotes) -> usize {
    let mut at = 0;
    let mut count = 0;
    while at + 8 <= data.len() {
        let property_type = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        let size = u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]);
        let start = at + 8;
        let end = start.saturating_add(size as usize);
        if end > data.len() {
            notes.truncated = true;
            break;
        }
        count += 1;
        if notes.properties.len() < MAX_PROPERTIES {
            notes.properties.push(ElfGnuProperty {
                property_type: property_type_name(property_type),
                data_size: size,
                data: hex_preview(&data[start..end]),
            });
        }
        else {
            notes.truncated = true;
        }
        at = align_u64(end as u64, 8) as usize;
    }
    if at < data.len() {
        notes.truncated = true;
    }
    count
}

fn note_type_name(owner: &str, kind: u64) -> String {
    if owner == "GNU" {
        match kind {
            NT_GNU_ABI_TAG => return "NT_GNU_ABI_TAG".to_string(),
            NT_GNU_BUILD_ID => return "NT_GNU_BUILD_ID".to_string(),
            NT_GNU_GOLD_VERSION => return "NT_GNU_GOLD_VERSION".to_string(),
            NT_GNU_PROPERTY_TYPE_0 => return "NT_GNU_PROPERTY_TYPE_0".to_string(),
            other => return format!("type {other}"),
        }
    }
    format!("type {kind}")
}

fn property_type_name(value: u32) -> String {
    match value {
        1 => "STACK_SIZE".to_string(),
        2 => "NO_COPY_ON_PROTECTED".to_string(),
        0xc000_0000 => "AARCH64_FEATURE_1_AND".to_string(),
        0xc000_0001 => "AARCH64_PAUTH_1_AND".to_string(),
        0xc000_0002 => "X86_FEATURE_1_AND".to_string(),
        0xc000_8001 => "X86_FEATURE_2_NEEDED".to_string(),
        0xc000_8002 => "X86_ISA_1_NEEDED".to_string(),
        0xc001_0001 => "X86_FEATURE_2_USED".to_string(),
        0xc001_0002 => "X86_ISA_1_USED".to_string(),
        other => format!("0x{other:08x}"),
    }
}

fn abi_os_name(os: u32) -> String {
    match os {
        0 => "Linux".to_string(),
        1 => "Hurd".to_string(),
        2 => "Solaris".to_string(),
        3 => "FreeBSD".to_string(),
        other => format!("#{other}"),
    }
}

fn hex_preview(data: &[u8]) -> String {
    hex_preview_bounded(data, MAX_DESCRIPTION_PREVIEW)
}

fn hex_preview_bounded(data: &[u8], limit: usize) -> String {
    let shown = data.len().min(limit);
    let mut text = hex_lower(&data[..shown]);
    if data.len() > limit {
        text.push_str("...");
    }
    text
}

fn hex_lower(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(data.len() * 2);
    for byte in data {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    text
}

fn align_u64(value: u64, alignment: u64) -> u64 {
    value.saturating_add(alignment - 1) & !(alignment - 1)
}
