//! CFF (Compact Font Format, version 1) outline support.
//!
//! This module implements the parts of CFF that produce glyph outlines:
//! header, INDEX structures, Top DICT, Private DICT, `CharStrings`, global and
//! local subroutines, CID-keyed `FDArray`/`FDSelect`, and a Type 2 charstring
//! interpreter. `charset` and `Encoding` are intentionally skipped — Unicode
//! mapping comes from `cmap`, and glyph selection is by glyph id.
//!
//! CFF2 is not supported: it is a different, variation-oriented format and is
//! reported as [`CffError::Unsupported`] rather than guessed at.

use crate::{
    outline::{BBox, OutlineBuilder},
    reader::{ReadError, Reader},
};

/// CFF table header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CffInfo {
    /// Major version (1 for CFF Type1, 2 for CFF2).
    pub major: u8,
    /// Minor version (usually 0).
    pub minor: u8,
    /// OffsetSize (byte width of offset pointers).
    pub offset_size: u8,
    /// OffSize of the name INDEX.
    pub name_index_size: u8,
    /// Name INDEX length.
    pub name_index_count: u32,
}

/// One CFF INDEX table, kept for header-level introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CffIndex {
    /// OffSize (1..4).
    pub offset_size: u8,
    /// Number of entries.
    pub count: u32,
    /// Start offsets (count + 1 entries).
    pub offsets: Vec<u32>,
}

/// Try to read the CFF table header. Returns `None` if the blob is too small
/// or not CFF1.
pub fn parse_cff_header(data: &[u8]) -> Option<CffInfo> {
    if data.len() < 6 {
        return None;
    }
    let mut r = Reader::new(data);
    let major = r.read_u8().ok()?;
    let minor = r.read_u8().ok()?;
    let offset_size = r.read_u8().ok()?;
    let _ = r.read_u8(); // header size

    if major != 1 || !(1..=4).contains(&offset_size) {
        return None;
    }

    let name_index_size = offset_size;
    let name_index_count = read_index_count(&mut r).ok()?;
    Some(CffInfo { major, minor, offset_size, name_index_size, name_index_count })
}

fn read_index_count(r: &mut Reader<'_>) -> Result<u32, ReadError> {
    let count = r.read_u16()? as u32;
    let off_size = r.read_u8()?;
    r.skip((count + 1) as usize * off_size as usize)?;
    Ok(count)
}

/// Errors produced while parsing or interpreting a CFF table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CffError {
    /// The table ended before a structure was complete.
    Truncated,
    /// A structure is structurally invalid.
    Malformed(&'static str),
    /// A CFF variant this crate does not implement (CFF2).
    Unsupported(&'static str),
}

impl std::fmt::Display for CffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("CFF table is truncated"),
            Self::Malformed(what) => write!(f, "malformed CFF structure: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported CFF variant: {what}"),
        }
    }
}

impl std::error::Error for CffError {}

/// Absolute byte ranges `(start, end)` of the entries in one INDEX.
type IndexEntries = Vec<(usize, usize)>;

#[derive(Debug, Clone, Default)]
struct PrivateDict {
    subrs: IndexEntries,
}

/// Parsed CFF1 table that can emit Type 2 charstring outlines.
#[derive(Debug, Clone)]
pub struct CffTable {
    data: Vec<u8>,
    char_strings: IndexEntries,
    global_subrs: IndexEntries,
    private: Option<PrivateDict>,
    fd_privates: Vec<PrivateDict>,
    fd_select: Vec<u8>,
    is_cid: bool,
}

impl CffTable {
    /// Parse the `CFF ` table payload.
    pub fn parse(data: &[u8]) -> Result<Self, CffError> {
        if data.len() < 4 {
            return Err(CffError::Truncated);
        }
        let major = data[0];
        if major == 2 {
            return Err(CffError::Unsupported("CFF2"));
        }
        if major != 1 {
            return Err(CffError::Unsupported("unknown CFF major version"));
        }
        let header_size = data[2] as usize;
        if header_size < 4 || header_size > data.len() {
            return Err(CffError::Malformed("header size"));
        }

        let (_, next) = parse_index(data, header_size)?;
        let (top_dicts, next) = parse_index(data, next)?;
        let (_strings, next) = parse_index(data, next)?;
        let (global_subrs, _) = parse_index(data, next)?;

        let (top_start, top_end) = *top_dicts.first().ok_or(CffError::Malformed("empty Top DICT INDEX"))?;
        let top = parse_dict(data, top_start, top_end)?;

        let char_strings_offset = dict_number(&top, 17).ok_or(CffError::Malformed("missing CharStrings"))? as usize;
        let (char_strings, _) = parse_index(data, char_strings_offset)?;

        let is_cid = top.iter().any(|(operator, _)| *operator == 1230);
        let mut private = None;
        let mut fd_privates = Vec::new();
        let mut fd_select = Vec::new();

        if is_cid {
            if let Some(fd_array_offset) = dict_number(&top, 1236) {
                let (font_dicts, _) = parse_index(data, fd_array_offset as usize)?;
                fd_privates.reserve(font_dicts.len());
                for (start, end) in &font_dicts {
                    let font_dict = parse_dict(data, *start, *end)?;
                    fd_privates.push(parse_private(data, &font_dict, *start)?);
                }
            }
            if let Some(fd_select_offset) = dict_number(&top, 1237) {
                fd_select = parse_fd_select(data, fd_select_offset as usize, char_strings.len())?;
            }
        }
        else {
            private = Some(parse_private(data, &top, 0)?);
        }

        Ok(Self { data: data.to_vec(), char_strings, global_subrs, private, fd_privates, fd_select, is_cid })
    }

    /// Number of glyphs in the `CharStrings` INDEX.
    #[inline]
    pub fn num_glyphs(&self) -> usize {
        self.char_strings.len()
    }

    /// Emit the outline of `glyph_id`.
    ///
    /// Returns `None` for an out-of-range glyph or a charstring that fails to
    /// interpret; an empty glyph (for example a space) returns `Some(())`
    /// without emitting any segments.
    pub fn outline_glyph(&self, glyph_id: u16, builder: &mut dyn OutlineBuilder) -> Option<()> {
        let (start, end) = *self.char_strings.get(glyph_id as usize)?;
        let code = self.data.get(start..end)?;
        let local_subrs: &[(usize, usize)] = self.private_for(glyph_id).map_or(&[], |private| private.subrs.as_slice());
        let mut interpreter = Type2 {
            data: &self.data,
            global_subrs: &self.global_subrs,
            builder,
            stack: Vec::new(),
            transient: [0.0; 32],
            hints: 0,
            width_seen: false,
            path_open: false,
            x: 0.0,
            y: 0.0,
            operations: 0,
        };
        interpreter.run(code, local_subrs, 0).ok()?;
        Some(())
    }

    /// Bounding box of `glyph_id` in font units.
    pub fn glyph_bbox(&self, glyph_id: u16) -> Option<BBox> {
        let mut collector = BBoxCollector::default();
        self.outline_glyph(glyph_id, &mut collector)?;
        collector.bbox()
    }

    fn private_for(&self, glyph_id: u16) -> Option<&PrivateDict> {
        if !self.is_cid {
            return self.private.as_ref();
        }
        let index = self.fd_select.get(glyph_id as usize).copied().unwrap_or(0) as usize;
        self.fd_privates.get(index)
    }
}

// ---- INDEX / DICT primitives ----

fn read_u8(data: &[u8], position: usize) -> Result<u8, CffError> {
    data.get(position).copied().ok_or(CffError::Truncated)
}

fn read_u16(data: &[u8], position: usize) -> Result<u16, CffError> {
    let bytes = data.get(position..position + 2).ok_or(CffError::Truncated)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], position: usize) -> Result<u32, CffError> {
    let bytes = data.get(position..position + 4).ok_or(CffError::Truncated)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Parse a CFF INDEX at `offset`, returning its entries and the offset just
/// past the INDEX.
fn parse_index(data: &[u8], offset: usize) -> Result<(IndexEntries, usize), CffError> {
    let count = read_u16(data, offset)? as usize;
    if count == 0 {
        return Ok((Vec::new(), offset + 2));
    }
    let offset_size = read_u8(data, offset + 2)? as usize;
    if !(1..=4).contains(&offset_size) {
        return Err(CffError::Malformed("INDEX offSize"));
    }
    let offsets_start = offset + 3;
    let offsets_len = (count + 1) * offset_size;
    let offsets_end = offsets_start.checked_add(offsets_len).ok_or(CffError::Truncated)?;
    if offsets_end > data.len() {
        return Err(CffError::Truncated);
    }
    let base = offsets_end - 1; // CFF offsets are 1-based from this byte.

    let mut offsets = Vec::with_capacity(count + 1);
    for index in 0..=count {
        let mut value = 0usize;
        for byte in 0..offset_size {
            value = (value << 8) | data[offsets_start + index * offset_size + byte] as usize;
        }
        offsets.push(value);
    }
    if offsets[0] == 0 {
        return Err(CffError::Malformed("INDEX offset is zero"));
    }

    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let start = base.checked_add(offsets[index]).ok_or(CffError::Truncated)?;
        let end = base.checked_add(offsets[index + 1]).ok_or(CffError::Truncated)?;
        if start > end || end > data.len() {
            return Err(CffError::Malformed("INDEX entry range"));
        }
        entries.push((start, end));
    }
    let next = base.checked_add(offsets[count]).ok_or(CffError::Truncated)?;
    Ok((entries, next))
}

/// Parsed DICT as `(operator, operands)` pairs. Two-byte operators are encoded
/// as `1200 + second byte`.
fn parse_dict(data: &[u8], start: usize, end: usize) -> Result<Vec<(u16, Vec<f64>)>, CffError> {
    if end > data.len() || start > end {
        return Err(CffError::Malformed("DICT range"));
    }
    let mut entries = Vec::new();
    let mut operands: Vec<f64> = Vec::new();
    let mut position = start;
    while position < end {
        let byte = data[position];
        position += 1;
        match byte {
            0..=21 => {
                let operator = if byte == 12 {
                    let second = read_u8(data, position)?;
                    position += 1;
                    1200 + second as u16
                }
                else {
                    byte as u16
                };
                entries.push((operator, std::mem::take(&mut operands)));
            }
            28 => {
                let value = read_u16(data, position)? as i16;
                position += 2;
                operands.push(value as f64);
            }
            29 => {
                let value = read_u32(data, position)? as i32;
                position += 4;
                operands.push(value as f64);
            }
            30 => {
                let (value, next) = parse_real(data, position, end)?;
                position = next;
                operands.push(value);
            }
            32..=246 => operands.push((byte as i32 - 139) as f64),
            247..=250 => {
                let next = read_u8(data, position)? as i32;
                position += 1;
                operands.push((((byte as i32 - 247) * 256) + next + 108) as f64);
            }
            251..=254 => {
                let next = read_u8(data, position)? as i32;
                position += 1;
                operands.push((-(((byte as i32 - 251) * 256) + next + 108)) as f64);
            }
            _ => return Err(CffError::Malformed("DICT operand")),
        }
    }
    Ok(entries)
}

/// Decode a CFF real number (nibble encoded) starting at `position`.
fn parse_real(data: &[u8], position: usize, end: usize) -> Result<(f64, usize), CffError> {
    let mut text = String::new();
    let mut cursor = position;
    while cursor < end {
        let byte = data[cursor];
        cursor += 1;
        for nibble in [byte >> 4, byte & 0x0F] {
            match nibble {
                0..=9 => text.push(char::from(b'0' + nibble)),
                0xA => text.push('.'),
                0xB => text.push('E'),
                0xC => text.push_str("E-"),
                0xE => text.push('-'),
                0xF => {
                    return text.parse::<f64>().map(|value| (value, cursor)).map_err(|_| CffError::Malformed("real number"));
                }
                _ => return Err(CffError::Malformed("real number nibble")),
            }
        }
    }
    Err(CffError::Truncated)
}

fn dict_number(dict: &[(u16, Vec<f64>)], operator: u16) -> Option<f64> {
    dict.iter().find(|(op, _)| *op == operator).and_then(|(_, operands)| operands.last().copied())
}

fn parse_private(data: &[u8], dict: &[(u16, Vec<f64>)], base: usize) -> Result<PrivateDict, CffError> {
    let Some((size, offset)) = dict.iter().find(|(op, _)| *op == 18).and_then(|(_, operands)| {
        if operands.len() >= 2 { Some((operands[operands.len() - 2] as usize, operands[operands.len() - 1] as usize)) } else { None }
    })
    else {
        return Ok(PrivateDict::default());
    };
    // `base` is used for Font DICTs, where Private offsets are absolute from
    // the CFF table; the operand is already absolute in both cases, so only
    // the size needs to be validated.
    let _ = base;
    if size == 0 || offset.checked_add(size).is_none_or(|end| end > data.len()) {
        return Ok(PrivateDict::default());
    }
    let private_end = offset + size;
    let entries = parse_dict(data, offset, private_end)?;
    let subrs = if let Some(subrs_offset) = dict_number(&entries, 19) {
        let absolute = offset.checked_add(subrs_offset as usize).ok_or(CffError::Truncated)?;
        parse_index(data, absolute)?.0
    }
    else {
        Vec::new()
    };
    Ok(PrivateDict { subrs })
}

/// Parse `FDSelect` (formats 0 and 3) into a per-glyph Font DICT index.
fn parse_fd_select(data: &[u8], offset: usize, num_glyphs: usize) -> Result<Vec<u8>, CffError> {
    let format = read_u8(data, offset)?;
    let mut select = vec![0u8; num_glyphs];
    match format {
        0 => {
            for (glyph, slot) in select.iter_mut().enumerate() {
                *slot = read_u8(data, offset + 1 + glyph)?;
            }
        }
        3 => {
            let ranges = read_u16(data, offset + 1)? as usize;
            let records_start = offset + 3;
            let sentinel = read_u16(data, records_start + ranges * 3)? as usize;
            // The sentinel is the first glyph id after the last range.
            for range in 0..ranges {
                let first = read_u16(data, records_start + range * 3)? as usize;
                let fd = read_u8(data, records_start + range * 3 + 2)?;
                let next = if range + 1 < ranges { read_u16(data, records_start + (range + 1) * 3)? as usize } else { sentinel };
                let end = next.min(sentinel).min(num_glyphs);
                for slot in select.iter_mut().take(end).skip(first) {
                    *slot = fd;
                }
            }
        }
        _ => return Err(CffError::Malformed("FDSelect format")),
    }
    Ok(select)
}

// ---- Type 2 charstring interpreter ----

fn bias(count: usize) -> i32 {
    if count < 1240 {
        107
    }
    else if count < 33900 {
        1131
    }
    else {
        32768
    }
}

struct Type2<'a> {
    data: &'a [u8],
    global_subrs: &'a [(usize, usize)],
    builder: &'a mut dyn OutlineBuilder,
    stack: Vec<f64>,
    transient: [f64; 32],
    hints: usize,
    width_seen: bool,
    path_open: bool,
    x: f64,
    y: f64,
    operations: usize,
}

impl Type2<'_> {
    fn run(&mut self, code: &[u8], local_subrs: &[(usize, usize)], depth: u8) -> Result<(), CffError> {
        if depth > 10 {
            return Ok(());
        }
        let mut position = 0usize;
        while position < code.len() {
            self.operations += 1;
            if self.operations > 500_000 {
                return Err(CffError::Malformed("charstring op budget"));
            }
            let byte = code[position];
            position += 1;
            match byte {
                32..=246 => self.stack.push((byte as i32 - 139) as f64),
                247..=250 => {
                    let next = *code.get(position).ok_or(CffError::Truncated)? as i32;
                    position += 1;
                    self.stack.push((((byte as i32 - 247) * 256) + next + 108) as f64);
                }
                251..=254 => {
                    let next = *code.get(position).ok_or(CffError::Truncated)? as i32;
                    position += 1;
                    self.stack.push((-(((byte as i32 - 251) * 256) + next + 108)) as f64);
                }
                28 => {
                    let bytes = code.get(position..position + 2).ok_or(CffError::Truncated)?;
                    position += 2;
                    self.stack.push(i16::from_be_bytes([bytes[0], bytes[1]]) as f64);
                }
                255 => {
                    let bytes = code.get(position..position + 4).ok_or(CffError::Truncated)?;
                    position += 4;
                    let raw = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    self.stack.push(raw as f64 / 65536.0);
                }
                1 | 3 | 18 | 23 => self.stems(),
                19 | 20 => {
                    self.stems();
                    let mask_len = self.hints.div_ceil(8);
                    position = position.checked_add(mask_len).ok_or(CffError::Truncated)?;
                    if position > code.len() {
                        return Err(CffError::Truncated);
                    }
                }
                4 => {
                    self.move_width(1);
                    let dy = self.take_number(0.0);
                    self.y += dy;
                    self.start_subpath();
                }
                21 => {
                    self.move_width(2);
                    let dx = self.take_number(0.0);
                    let dy = self.take_number(0.0);
                    self.x += dx;
                    self.y += dy;
                    self.start_subpath();
                }
                22 => {
                    self.move_width(1);
                    let dx = self.take_number(0.0);
                    self.x += dx;
                    self.start_subpath();
                }
                5 => {
                    self.width_seen = true;
                    while self.stack.len() >= 2 {
                        let dx = self.take_number(0.0);
                        let dy = self.take_number(0.0);
                        self.line_relative(dx, dy);
                    }
                    self.stack.clear();
                }
                6 => {
                    self.width_seen = true;
                    let mut horizontal = true;
                    while !self.stack.is_empty() {
                        let value = self.take_number(0.0);
                        if horizontal {
                            self.line_relative(value, 0.0);
                        }
                        else {
                            self.line_relative(0.0, value);
                        }
                        horizontal = !horizontal;
                    }
                    self.stack.clear();
                }
                7 => {
                    self.width_seen = true;
                    let mut vertical = true;
                    while !self.stack.is_empty() {
                        let value = self.take_number(0.0);
                        if vertical {
                            self.line_relative(0.0, value);
                        }
                        else {
                            self.line_relative(value, 0.0);
                        }
                        vertical = !vertical;
                    }
                    self.stack.clear();
                }
                8 => {
                    self.width_seen = true;
                    while self.stack.len() >= 6 {
                        let values = self.take_six();
                        self.curve_relative(values[0], values[1], values[2], values[3], values[4], values[5]);
                    }
                    self.stack.clear();
                }
                24 => {
                    self.width_seen = true;
                    while self.stack.len() >= 8 {
                        let values = self.take_six();
                        self.curve_relative(values[0], values[1], values[2], values[3], values[4], values[5]);
                    }
                    while self.stack.len() >= 2 {
                        let dx = self.take_number(0.0);
                        let dy = self.take_number(0.0);
                        self.line_relative(dx, dy);
                    }
                    self.stack.clear();
                }
                25 => {
                    self.width_seen = true;
                    while self.stack.len() >= 8 {
                        let dx = self.take_number(0.0);
                        let dy = self.take_number(0.0);
                        self.line_relative(dx, dy);
                    }
                    if self.stack.len() >= 6 {
                        let values = self.take_six();
                        self.curve_relative(values[0], values[1], values[2], values[3], values[4], values[5]);
                    }
                    self.stack.clear();
                }
                26 => self.vvcurveto(),
                27 => self.hhcurveto(),
                30 => self.alternating_curves(false),
                31 => self.alternating_curves(true),
                10 => {
                    let Some(index) = self.stack.pop()
                    else {
                        continue;
                    };
                    let target = index as i32 + bias(local_subrs.len());
                    let Some((start, end)) = usize::try_from(target).ok().and_then(|index| local_subrs.get(index).copied())
                    else {
                        continue;
                    };
                    let data = self.data;
                    if let Some(subroutine) = data.get(start..end) {
                        self.run(subroutine, local_subrs, depth + 1)?;
                    }
                }
                29 => {
                    let Some(index) = self.stack.pop()
                    else {
                        continue;
                    };
                    let target = index as i32 + bias(self.global_subrs.len());
                    let Some((start, end)) = usize::try_from(target).ok().and_then(|index| self.global_subrs.get(index).copied())
                    else {
                        continue;
                    };
                    let data = self.data;
                    if let Some(subroutine) = data.get(start..end) {
                        self.run(subroutine, local_subrs, depth + 1)?;
                    }
                }
                11 => return Ok(()),
                14 => {
                    if !self.width_seen && matches!(self.stack.len(), 1 | 5) {
                        self.stack.remove(0);
                    }
                    if self.path_open {
                        self.builder.close();
                        self.path_open = false;
                    }
                    self.stack.clear();
                    return Ok(());
                }
                12 => {
                    let escape = *code.get(position).ok_or(CffError::Truncated)?;
                    position += 1;
                    self.escape(escape);
                }
                _ => self.stack.clear(),
            }
        }
        Ok(())
    }

    fn stems(&mut self) {
        if !self.width_seen {
            if self.stack.len() % 2 == 1 {
                self.stack.remove(0);
            }
            self.width_seen = true;
        }
        self.hints += self.stack.len() / 2;
        self.stack.clear();
    }

    fn move_width(&mut self, arguments: usize) {
        if !self.width_seen && self.stack.len() > arguments {
            self.stack.remove(0);
        }
        self.width_seen = true;
    }

    /// Pop the first remaining number, or `default` when the stack is empty.
    fn take_number(&mut self, default: f64) -> f64 {
        if self.stack.is_empty() { default } else { self.stack.remove(0) }
    }

    fn take_six(&mut self) -> [f64; 6] {
        let mut values = [0.0; 6];
        for value in &mut values {
            *value = self.take_number(0.0);
        }
        values
    }

    /// Finish the open subpath (if any) and start a new one at the current point.
    fn start_subpath(&mut self) {
        if self.path_open {
            self.builder.close();
        }
        self.path_open = true;
        self.builder.move_to(self.x as f32, self.y as f32);
    }

    fn line_relative(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.path_open = true;
        self.builder.line_to(self.x as f32, self.y as f32);
    }

    fn curve_relative(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        self.path_open = true;
        let x1 = self.x + dx1;
        let y1 = self.y + dy1;
        let x2 = x1 + dx2;
        let y2 = y1 + dy2;
        let x3 = x2 + dx3;
        let y3 = y2 + dy3;
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
        self.x = x3;
        self.y = y3;
    }

    fn hhcurveto(&mut self) {
        self.width_seen = true;
        let mut dy1 = 0.0;
        if self.stack.len() % 4 == 1 {
            dy1 = self.take_number(0.0);
        }
        while self.stack.len() >= 4 {
            let dx1 = self.take_number(0.0);
            let dx2 = self.take_number(0.0);
            let dy2 = self.take_number(0.0);
            let dx3 = self.take_number(0.0);
            let x1 = self.x + dx1;
            let y1 = self.y + dy1;
            let x2 = x1 + dx2;
            let y2 = y1 + dy2;
            let x3 = x2 + dx3;
            let y3 = y2;
            self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
            self.x = x3;
            self.y = y3;
            dy1 = 0.0;
        }
        self.stack.clear();
    }

    fn vvcurveto(&mut self) {
        self.width_seen = true;
        let mut dx1 = 0.0;
        if self.stack.len() % 4 == 1 {
            dx1 = self.take_number(0.0);
        }
        while self.stack.len() >= 4 {
            let dy1 = self.take_number(0.0);
            let dx2 = self.take_number(0.0);
            let dy2 = self.take_number(0.0);
            let dy3 = self.take_number(0.0);
            let x1 = self.x + dx1;
            let y1 = self.y + dy1;
            let x2 = x1 + dx2;
            let y2 = y1 + dy2;
            let x3 = x2;
            let y3 = y2 + dy3;
            self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
            self.x = x3;
            self.y = y3;
            dx1 = 0.0;
        }
        self.stack.clear();
    }

    /// `hvcurveto` (horizontal first) and `vhcurveto` (vertical first).
    fn alternating_curves(&mut self, horizontal_first: bool) {
        self.width_seen = true;
        let arguments = std::mem::take(&mut self.stack);
        let mut index = 0usize;
        let mut horizontal = horizontal_first;
        let trailing = arguments.len() % 4 == 1;
        let groups = arguments.len() / 4;
        for group in 0..groups {
            let a = arguments[index];
            let b = arguments[index + 1];
            let c = arguments[index + 2];
            let d = arguments[index + 3];
            index += 4;
            let extra = if trailing && group + 1 == groups { arguments[index] } else { 0.0 };
            let (x1, y1, x2, y2, x3, y3) = if horizontal {
                let x1 = self.x + a;
                let y1 = self.y;
                let x2 = x1 + b;
                let y2 = y1 + c;
                (x1, y1, x2, y2, x2 + extra, y2 + d)
            }
            else {
                let x1 = self.x;
                let y1 = self.y + a;
                let x2 = x1 + b;
                let y2 = y1 + c;
                (x1, y1, x2, y2, x2 + d, y2 + extra)
            };
            self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
            self.x = x3;
            self.y = y3;
            horizontal = !horizontal;
        }
    }

    fn escape(&mut self, operator: u8) {
        match operator {
            3 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
            }
            4 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 });
            }
            9 => {
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a.abs());
            }
            10 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a + b);
            }
            11 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a - b);
            }
            12 => {
                let b = self.pop().unwrap_or(1.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(if b == 0.0 { 0.0 } else { a / b });
            }
            14 => {
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(-a);
            }
            18 => {
                self.pop();
            }
            24 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a * b);
            }
            26 => {
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a.max(0.0).sqrt());
            }
            27 => {
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(a);
                self.stack.push(a);
            }
            28 => {
                let b = self.pop().unwrap_or(0.0);
                let a = self.pop().unwrap_or(0.0);
                self.stack.push(b);
                self.stack.push(a);
            }
            34 => self.hflex(),
            35 => self.flex(),
            36 => self.hflex1(),
            37 => self.flex1(),
            _ => self.stack.clear(),
        }
    }

    fn pop(&mut self) -> Option<f64> {
        self.stack.pop()
    }

    fn flex(&mut self) {
        if self.stack.len() < 13 {
            self.stack.clear();
            return;
        }
        let values: Vec<f64> = self.stack.drain(..13).collect();
        self.curve_relative(values[0], values[1], values[2], values[3], values[4], values[5]);
        self.curve_relative(values[6], values[7], values[8], values[9], values[10], values[11]);
        self.stack.clear();
    }

    fn hflex(&mut self) {
        if self.stack.len() < 8 {
            self.stack.clear();
            return;
        }
        let y_start = self.y;
        let values: Vec<f64> = self.stack.drain(..8).collect();
        let x1 = self.x + values[0];
        let y1 = self.y;
        let x2 = x1 + values[1];
        let y2 = y1 + values[2];
        let x3 = x2 + values[3];
        let y3 = y2;
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
        self.x = x3;
        self.y = y3;

        let x1 = self.x + values[4];
        let y1 = self.y;
        let x2 = x1 + values[5];
        let y2 = y1 + values[6];
        let x3 = x2 + values[7];
        // A flex must close: the second curve ends back on the starting
        // horizontal line even if the deltas do not sum to exactly zero.
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y_start as f32);
        self.x = x3;
        self.y = y_start;
        self.stack.clear();
    }

    fn hflex1(&mut self) {
        if self.stack.len() < 9 {
            self.stack.clear();
            return;
        }
        let y_start = self.y;
        let values: Vec<f64> = self.stack.drain(..9).collect();
        let x1 = self.x + values[0];
        let y1 = self.y + values[1];
        let x2 = x1 + values[2];
        let y2 = y1 + values[3];
        let x3 = x2 + values[4];
        let y3 = y2;
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
        self.x = x3;
        self.y = y3;

        let x1 = self.x + values[5];
        let y1 = self.y;
        let x2 = x1 + values[6];
        let y2 = y1 + values[7];
        let x3 = x2 + values[8];
        // The implicit final `-(dy1 + dy2 + dy5)` returns to the starting y.
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y_start as f32);
        self.x = x3;
        self.y = y_start;
        self.stack.clear();
    }

    fn flex1(&mut self) {
        if self.stack.len() < 11 {
            self.stack.clear();
            return;
        }
        let (x_start, y_start) = (self.x, self.y);
        let values: Vec<f64> = self.stack.drain(..11).collect();
        let dx = values[0] + values[2] + values[4] + values[6] + values[8];
        let dy = values[1] + values[3] + values[5] + values[7] + values[9];
        let x1 = self.x + values[0];
        let y1 = self.y + values[1];
        let x2 = x1 + values[2];
        let y2 = y1 + values[3];
        let x3 = x2 + values[4];
        let y3 = y2 + values[5];
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
        self.x = x3;
        self.y = y3;

        let x1 = self.x + values[6];
        let y1 = self.y + values[7];
        let x2 = x1 + values[8];
        let y2 = y1 + values[9];
        // The flex axis decides which of the two starting coordinates the
        // last point snaps back to; the perpendicular delta is implicit.
        let (x3, y3) = if dx.abs() > dy.abs() { (x2 + values[10], y_start) } else { (x_start, y2 + values[10]) };
        self.builder.curve_to(x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32);
        self.x = x3;
        self.y = y3;
        self.stack.clear();
    }
}

/// Collects the extents of every point handed to an [`OutlineBuilder`].
#[derive(Debug, Clone, Copy)]
struct BBoxCollector {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    seen: bool,
}

impl Default for BBoxCollector {
    fn default() -> Self {
        Self { min_x: f32::INFINITY, min_y: f32::INFINITY, max_x: f32::NEG_INFINITY, max_y: f32::NEG_INFINITY, seen: false }
    }
}

impl BBoxCollector {
    fn include(&mut self, x: f32, y: f32) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
        self.seen = true;
    }

    fn bbox(&self) -> Option<BBox> {
        if !self.seen {
            return None;
        }
        let clamp = |value: f32| value.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        Some(BBox {
            x_min: clamp(self.min_x.floor()),
            y_min: clamp(self.min_y.floor()),
            x_max: clamp(self.max_x.ceil()),
            y_max: clamp(self.max_y.ceil()),
        })
    }
}

impl OutlineBuilder for BBoxCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.include(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.include(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.include(x1, y1);
        self.include(x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.include(x1, y1);
        self.include(x2, y2);
        self.include(x, y);
    }

    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the outline calls so the tests can assert exact geometry.
    #[derive(Default)]
    struct Recorder {
        lines: Vec<(f32, f32)>,
        curves: Vec<([f32; 2], [f32; 2], [f32; 2])>,
        closes: usize,
        last: (f32, f32),
    }

    impl OutlineBuilder for Recorder {
        fn move_to(&mut self, x: f32, y: f32) {
            self.last = (x, y);
        }

        fn line_to(&mut self, x: f32, y: f32) {
            self.lines.push((x, y));
            self.last = (x, y);
        }

        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.curves.push(([x1, y1], [x1, y1], [x, y]));
            self.last = (x, y);
        }

        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            self.curves.push(([x1, y1], [x2, y2], [x, y]));
            self.last = (x, y);
        }

        fn close(&mut self) {
            self.closes += 1;
        }
    }

    fn number(value: i32) -> Vec<u8> {
        let mut out = vec![28];
        out.extend_from_slice(&(value as i16).to_be_bytes());
        out
    }

    fn operands(values: &[i32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend(number(*value));
        }
        out
    }

    /// Run a charstring through the interpreter without a CFF container.
    fn run(code: &[u8]) -> Result<Recorder, CffError> {
        let data = code.to_vec();
        let mut recorder = Recorder::default();
        {
            let mut interpreter = Type2 {
                data: &data,
                global_subrs: &[],
                builder: &mut recorder,
                stack: Vec::new(),
                transient: [0.0; 32],
                hints: 0,
                width_seen: false,
                path_open: false,
                x: 0.0,
                y: 0.0,
                operations: 0,
            };
            interpreter.run(code, &[], 0)?;
        }
        Ok(recorder)
    }

    #[test]
    fn subr_bias_matches_the_type2_table() {
        assert_eq!(bias(0), 107);
        assert_eq!(bias(1239), 107);
        assert_eq!(bias(1240), 1131);
        assert_eq!(bias(33_899), 1131);
        assert_eq!(bias(33_900), 32_768);
    }

    #[test]
    fn fd_select_formats_map_every_glyph() {
        assert_eq!(parse_fd_select(&[0, 0, 1, 1, 1], 0, 4).expect("format 0"), vec![0, 1, 1, 1]);
        // Format 3: glyphs 0..2 use FD 0, glyphs 2..4 use FD 1.
        let format_3 = [3, 0, 2, 0, 0, 0, 0, 2, 1, 0, 4];
        assert_eq!(parse_fd_select(&format_3, 0, 4).expect("format 3"), vec![0, 0, 1, 1]);
        assert!(parse_fd_select(&[9], 0, 1).is_err(), "unknown FDSelect format");
        assert!(parse_fd_select(&[0, 1], 0, 4).is_err(), "format 0 must cover every glyph");
    }

    #[test]
    fn hflex_ends_on_the_starting_baseline() {
        let mut code = operands(&[0, 0]);
        code.push(21);
        code.extend(operands(&[10, 20, 2, 30, 40, 50, 3, 60]));
        code.extend([12, 34]);
        let recorder = run(&code).expect("hflex");
        assert_eq!(recorder.curves.len(), 2);
        assert_eq!(recorder.last, (210.0, 0.0), "flex must close back onto y=0");
    }

    #[test]
    fn hflex1_forces_the_implicit_closing_delta() {
        let mut code = operands(&[0, 0]);
        code.push(21);
        // dy1 + dy2 + dy5 = 6, so the implicit final delta must bring y back to 0.
        code.extend(operands(&[10, 1, 20, 2, 30, 40, 50, 3, 60]));
        code.extend([12, 36]);
        let recorder = run(&code).expect("hflex1");
        assert_eq!(recorder.curves.len(), 2);
        assert_eq!(recorder.last, (210.0, 0.0));
    }

    #[test]
    fn flex1_snaps_the_perpendicular_axis_to_the_start() {
        let mut code = operands(&[0, 0]);
        code.push(21);
        // dx = 150, dy = 3: the horizontal axis wins and y returns to 0.
        code.extend(operands(&[10, 1, 20, 1, 30, 0, 40, 0, 50, 1, 7]));
        code.extend([12, 37]);
        let recorder = run(&code).expect("flex1");
        assert_eq!(recorder.curves.len(), 2);
        assert_eq!(recorder.last, (157.0, 0.0));

        // The vertical branch snaps x back to the start instead.
        let mut code = operands(&[0, 0]);
        code.push(21);
        code.extend(operands(&[1, 10, 2, 20, 0, 30, 0, 20, 1, 19, 7]));
        code.extend([12, 37]);
        let recorder = run(&code).expect("flex1 vertical");
        assert_eq!(recorder.curves.len(), 2);
        assert_eq!(recorder.last, (0.0, 106.0));
    }

    #[test]
    fn flex_draws_two_curves_before_the_flex_depth() {
        let mut code = operands(&[0, 0]);
        code.push(21);
        code.extend(operands(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 50]));
        code.extend([12, 35]);
        let recorder = run(&code).expect("flex");
        assert_eq!(recorder.curves.len(), 2);
        assert_eq!(recorder.last, (36.0, 42.0));
    }

    #[test]
    fn truncated_charstrings_return_errors_instead_of_panicking() {
        assert!(matches!(run(&[28, 0]), Err(CffError::Truncated)));
        assert!(matches!(run(&[12]), Err(CffError::Truncated)));
        assert!(matches!(run(&[255, 0, 0]), Err(CffError::Truncated)));
    }

    #[test]
    fn cff2_and_short_headers_are_rejected() {
        assert!(matches!(CffTable::parse(&[2, 0, 4, 4]), Err(CffError::Unsupported(_))));
        assert!(matches!(CffTable::parse(&[1, 0]), Err(CffError::Truncated)));
    }

    // ---- synthetic CID-keyed CFF end to end ----

    fn cff_index(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(items.len() as u16).to_be_bytes());
        if items.is_empty() {
            return out;
        }
        out.push(4);
        let mut offset = 1u32;
        out.extend_from_slice(&offset.to_be_bytes());
        for item in items {
            offset += item.len() as u32;
            out.extend_from_slice(&offset.to_be_bytes());
        }
        for item in items {
            out.extend_from_slice(item);
        }
        out
    }

    fn dict_entry(operator: u16, values: &[i32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.push(29);
            out.extend_from_slice(&value.to_be_bytes());
        }
        if operator < 1200 {
            out.push(operator as u8);
        }
        else {
            out.push(12);
            out.push((operator - 1200) as u8);
        }
        out
    }

    fn glyph_with_local_subr() -> Vec<u8> {
        let mut code = operands(&[0, 0]);
        code.push(21);
        code.extend(operands(&[100, 0]));
        code.push(5);
        // One local subr means a bias of 107.
        code.extend(number(-107));
        code.push(10);
        code.push(14);
        code
    }

    fn closing_subr() -> Vec<u8> {
        let mut code = operands(&[0, 100]);
        code.push(5);
        code.extend(operands(&[-100, 0]));
        code.push(5);
        code.push(11);
        code
    }

    /// Build a CID-keyed CFF whose glyph 1 draws its top edge through the
    /// local subroutine of FD 1, so a wrong FDSelect mapping loses that edge.
    fn cid_cff(fd_select: &[u8]) -> Vec<u8> {
        let header = vec![1u8, 0, 4, 4];
        let name = cff_index(&[b"CIDTest".to_vec()]);
        let strings = cff_index(&[]);
        let global_subrs = cff_index(&[]);
        let charstrings = cff_index(&[vec![14], glyph_with_local_subr()]);
        let top_length =
            dict_entry(1230, &[0, 0, 0]).len() + dict_entry(17, &[0]).len() + dict_entry(1236, &[0]).len() + dict_entry(1237, &[0]).len();
        let top_index_length = 2 + 1 + 2 * 4 + top_length;
        let charstrings_offset = header.len() + name.len() + top_index_length + strings.len() + global_subrs.len();
        let fdarray_offset = charstrings_offset + charstrings.len();
        // Two Font DICTs of eleven bytes each.
        let fdarray_length = 2 + 1 + 3 * 4 + 22;
        let fdselect_offset = fdarray_offset + fdarray_length;
        let private_offset = fdselect_offset + fd_select.len();
        let private_length = dict_entry(19, &[0]).len();
        let subrs_offset = private_offset + private_length;

        let private = dict_entry(19, &[private_length as i32]);
        let font_dict_0 = dict_entry(18, &[0, 0]);
        let font_dict_1 = dict_entry(18, &[private_length as i32, private_offset as i32]);
        let fdarray = cff_index(&[font_dict_0, font_dict_1]);
        let local_subrs = cff_index(&[closing_subr()]);
        let top = [
            dict_entry(1230, &[0, 0, 0]),
            dict_entry(17, &[charstrings_offset as i32]),
            dict_entry(1236, &[fdarray_offset as i32]),
            dict_entry(1237, &[fdselect_offset as i32]),
        ]
        .concat();

        let mut out = header;
        out.extend(name);
        out.extend(cff_index(&[top]));
        out.extend(strings);
        out.extend(global_subrs);
        out.extend(charstrings);
        out.extend(fdarray);
        out.extend_from_slice(fd_select);
        out.extend(private);
        out.extend(local_subrs);
        assert_eq!(out.len(), subrs_offset + cff_index(&[closing_subr()]).len());
        out
    }

    #[test]
    fn cid_fd_select_routes_glyphs_to_their_private_subrs() {
        for fd_select in [vec![0u8, 0, 1], vec![3, 0, 2, 0, 0, 0, 0, 1, 1, 0, 2]] {
            let table = CffTable::parse(&cid_cff(&fd_select)).expect("parse CID CFF");
            assert_eq!(table.num_glyphs(), 2);
            let bbox = table.glyph_bbox(1).expect("glyph 1 bbox");
            assert_eq!((bbox.x_min, bbox.y_min, bbox.x_max, bbox.y_max), (0, 0, 100, 100), "FDSelect {fd_select:?}");
            let mut recorder = Recorder::default();
            assert!(table.outline_glyph(1, &mut recorder).is_some());
            assert!(recorder.lines.contains(&(100.0, 100.0)), "FD 1 owns the subroutine that draws the top edge");
            assert!(table.glyph_bbox(0).is_none(), ".notdef is empty");
        }
    }
}
