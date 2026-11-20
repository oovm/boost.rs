//! Baseline sequential JPEG decoder (ITU-T T.81), written from the
//! specification.
//!
//! Supported: 8-bit samples, one scan, 1 or 3 components, Huffman entropy
//! coding, sampling 4:4:4 / 4:2:2 / 4:2:0 / 4:4:0, restart markers, and the
//! APP1 `Orientation` tag. Progressive (SOF2), extended sequential (SOF1),
//! lossless, arithmetic-coded and multi-scan streams fail with an explicit
//! [`DecodeError::Unsupported`].

use super::{DecodeError, DecodeLimits, DecodedImage, Orientation, exif::orientation_from_app1};
use crate::Bitmap;

/// The two-byte JPEG start-of-image marker.
pub(crate) const SIGNATURE: &[u8; 2] = &[0xFF, 0xD8];

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DHT: u8 = 0xC4;
const DQT: u8 = 0xDB;
const DRI: u8 = 0xDD;
const APP1: u8 = 0xE1;

/// Decodes a baseline sequential JPEG file into an RGBA8 [`Bitmap`].
///
/// The `max_pixels` budget is checked against the `SOFn` header before any
/// plane is allocated. EXIF orientation is reported, never applied.
pub fn decode_jpeg(bytes: &[u8], limits: DecodeLimits) -> Result<DecodedImage, DecodeError> {
    if !bytes.starts_with(SIGNATURE) {
        return Err(DecodeError::WrongSignature);
    }

    let mut orientation = Orientation::TopLeft;
    let mut quant = [[0u16; 64]; 4];
    let mut dc_tables: [Option<HuffmanTable>; 4] = std::array::from_fn(|_| None);
    let mut ac_tables: [Option<HuffmanTable>; 4] = std::array::from_fn(|_| None);
    let mut frame: Option<Frame> = None;
    let mut restart_interval = 0usize;
    let mut scan_done = false;

    let mut pos = SIGNATURE.len();
    loop {
        let (marker, after_marker) = read_marker(bytes, pos)?;
        pos = after_marker;
        match marker {
            EOI => break,
            SOS => {
                if scan_done {
                    return Err(DecodeError::Unsupported("multi-scan JPEG"));
                }
                let (payload, payload_end) = segment(bytes, pos)?;
                let scan = {
                    let frame = frame.as_ref().ok_or(DecodeError::Malformed("SOS before SOF"))?;
                    read_scan_header(payload, frame)?
                };
                pos = payload_end;
                let reader = ScanReader::new(&bytes[payload_end..]);
                let frame = frame.as_mut().ok_or(DecodeError::Malformed("SOS before SOF"))?;
                let reader = decode_scan(reader, frame, &scan, &quant, &dc_tables, &ac_tables)?;
                scan_done = true;
                pos += reader.pos;
                break;
            }
            DQT => {
                let (payload, next) = segment(bytes, pos)?;
                pos = next;
                read_quantization(payload, &mut quant)?;
            }
            DHT => {
                let (payload, next) = segment(bytes, pos)?;
                pos = next;
                read_huffman(payload, &mut dc_tables, &mut ac_tables)?;
            }
            DRI => {
                let (payload, next) = segment(bytes, pos)?;
                pos = next;
                if payload.len() < 2 {
                    return Err(DecodeError::Truncated { offset: pos });
                }
                restart_interval = usize::from(be16(payload, 0));
            }
            APP1 => {
                let (payload, next) = segment(bytes, pos)?;
                pos = next;
                if let Some(found) = orientation_from_app1(payload) {
                    orientation = found;
                }
            }
            0xC0 => {
                let (payload, next) = segment(bytes, pos)?;
                pos = next;
                if frame.is_some() {
                    return Err(DecodeError::Malformed("duplicate SOF"));
                }
                let parsed = read_frame(payload, limits)?;
                limits.check_size(parsed.width, parsed.height)?;
                frame = Some(parsed);
            }
            0xC1 => return Err(DecodeError::Unsupported("extended sequential DCT (SOF1)")),
            0xC2 => return Err(DecodeError::Unsupported("progressive DCT (SOF2)")),
            0xC3 => return Err(DecodeError::Unsupported("lossless JPEG (SOF3)")),
            0xC5 | 0xC6 | 0xC7 => return Err(DecodeError::Unsupported("differential JPEG (SOF5-SOF7)")),
            0xC9 | 0xCA | 0xCB => return Err(DecodeError::Unsupported("12-bit sequential JPEG (SOF9-SOF11)")),
            0xCD | 0xCE | 0xCF => return Err(DecodeError::Unsupported("differential JPEG (SOF13-SOF15)")),
            0xC8 => return Err(DecodeError::Unsupported("arithmetic-coded entropy (JOIF)")),
            0x01 => {}
            0xCC => return Err(DecodeError::Unsupported("arithmetic coding conditioning (DAC)")),
            0xD8 => return Err(DecodeError::Malformed("nested SOI")),
            0xDC => return Err(DecodeError::Unsupported("JPEG data length definition (DNL)")),
            0xDD..=0xDF => return Err(DecodeError::Unsupported("reserved JPEG marker class")),
            _ if (0xD0..=0xD7).contains(&marker) => return Err(DecodeError::Malformed("restart marker outside a scan")),
            _ if (0xE0..=0xEF).contains(&marker) || marker == 0xFE => {
                let (_, next) = segment(bytes, pos)?;
                pos = next;
            }
            _ => return Err(DecodeError::Malformed("unknown JPEG marker")),
        }
    }

    let mut frame = frame.ok_or(DecodeError::Malformed("JPEG without SOF"))?;
    if !scan_done {
        return Err(DecodeError::Malformed("JPEG without SOS"));
    }
    let bitmap = frame.compose()?;
    Ok(DecodedImage { bitmap, orientation })
}

/// Reads the `FF xx` marker at `pos`, skipping fill `FF` bytes.
fn read_marker(data: &[u8], pos: usize) -> Result<(u8, usize), DecodeError> {
    if pos >= data.len() {
        return Err(DecodeError::Truncated { offset: pos });
    }
    if data[pos] != 0xFF {
        return Err(DecodeError::Malformed("expected a marker where segment data continues"));
    }
    let mut p = pos + 1;
    while p < data.len() && data[p] == 0xFF {
        p += 1;
    }
    if p >= data.len() {
        return Err(DecodeError::Truncated { offset: p });
    }
    Ok((data[p], p + 1))
}

/// Reads a length-prefixed segment starting just after its marker byte.
fn segment(data: &[u8], pos: usize) -> Result<(&[u8], usize), DecodeError> {
    if pos + 2 > data.len() {
        return Err(DecodeError::Truncated { offset: pos });
    }
    let length = usize::from(be16(data, pos));
    if length < 2 {
        return Err(DecodeError::Malformed("segment length shorter than its own header"));
    }
    let end = pos + length;
    if end > data.len() {
        return Err(DecodeError::Truncated { offset: data.len() });
    }
    Ok((&data[pos + 2..end], end))
}

/// One image frame: dimensions, components and their sampling.
struct Frame {
    width: u32,
    height: u32,
    components: Vec<Component>,
    max_h: usize,
    max_v: usize,
}

/// A component plane with its sampling factors and table ids.
struct Component {
    /// Component identifier as stored in the frame header.
    id: u8,
    h: usize,
    v: usize,
    qt: usize,
    dc: usize,
    ac: usize,
    plane: Vec<u8>,
    pw: usize,
    ph: usize,
}

fn read_frame(data: &[u8], limits: DecodeLimits) -> Result<Frame, DecodeError> {
    let _ = limits;
    if data.len() < 8 {
        return Err(DecodeError::Truncated { offset: 0 });
    }
    if data[0] != 8 {
        return Err(DecodeError::Unsupported("JPEG sample precision other than 8 bits"));
    }
    let height = u32::from(be16(data, 1));
    let width = u32::from(be16(data, 3));
    if width == 0 || height == 0 {
        return Err(DecodeError::Malformed("zero-sized JPEG frame"));
    }
    let count = usize::from(data[5]);
    if count != 1 && count != 3 {
        return Err(DecodeError::Unsupported("JPEG with 2 or 4 colour components"));
    }
    if data.len() < 6 + count * 3 {
        return Err(DecodeError::Truncated { offset: data.len() });
    }
    let mut components = Vec::with_capacity(count);
    let mut max_h = 1usize;
    let mut max_v = 1usize;
    for i in 0..count {
        let base = 6 + i * 3;
        let sampling = data[base + 1];
        let h = (sampling >> 4) as usize;
        let v = (sampling & 0x0F) as usize;
        if !matches!((h, v), (1, 1) | (2, 1) | (1, 2) | (2, 2)) {
            return Err(DecodeError::Unsupported("JPEG chroma sampling outside 4:4:4, 4:2:2, 4:2:0 and 4:4:0"));
        }
        let qt = usize::from(data[base + 2]);
        if qt > 3 {
            return Err(DecodeError::Malformed("quantisation table id out of range"));
        }
        let id = data[base];
        if components.iter().any(|component: &Component| component.id == id) {
            return Err(DecodeError::Malformed("duplicate component id"));
        }
        max_h = max_h.max(h);
        max_v = max_v.max(v);
        components.push(Component { id, h, v, qt, dc: 0, ac: 0, plane: Vec::new(), pw: 0, ph: 0 });
    }
    for component in &mut components {
        component.pw = (width as usize * component.h).div_ceil(max_h);
        component.ph = (height as usize * component.v).div_ceil(max_v);
        component.plane = vec![0u8; component.pw * component.ph];
    }
    Ok(Frame { width, height, components, max_h, max_v })
}

/// One component's entry in the scan header.
struct ScanComponent {
    /// Index into [`Frame::components`].
    component: usize,
    /// DC Huffman table id.
    dc: usize,
    /// AC Huffman table id.
    ac: usize,
}

/// The scan header: which components take part and their Huffman tables.
struct Scan {
    order: Vec<ScanComponent>,
}

fn read_scan_header(data: &[u8], frame: &Frame) -> Result<Scan, DecodeError> {
    if data.is_empty() {
        return Err(DecodeError::Truncated { offset: 0 });
    }
    let ns = usize::from(data[0]);
    if ns != frame.components.len() {
        return Err(DecodeError::Unsupported("multi-scan JPEG"));
    }
    if data.len() < 1 + ns * 2 + 3 {
        return Err(DecodeError::Truncated { offset: data.len() });
    }
    let mut order = Vec::with_capacity(ns);
    let mut seen = vec![false; frame.components.len()];
    for i in 0..ns {
        let selector = usize::from(data[1 + i * 2]);
        let slot = usize::from(data[1 + i * 2 + 1]);
        let dc = (slot >> 4) as usize;
        let ac = (slot & 0x0F) as usize;
        if dc > 3 || ac > 3 {
            return Err(DecodeError::Malformed("Huffman table id out of range"));
        }
        let Some(component) = frame.components.iter().position(|c| usize::from(c.id) == selector)
        else {
            return Err(DecodeError::Malformed("scan references an unknown component"));
        };
        if std::mem::replace(&mut seen[component], true) {
            return Err(DecodeError::Malformed("component listed twice in one scan"));
        }
        order.push(ScanComponent { component, dc, ac });
    }
    let spectral_start = data[1 + ns * 2];
    let spectral_end = data[2 + ns * 2];
    let approximation = data[3 + ns * 2];
    if spectral_start != 0 || spectral_end != 63 || approximation != 0 {
        return Err(DecodeError::Unsupported("progressive or successive-approximation scan"));
    }
    Ok(Scan { order })
}

/// Reads `DQT` payload: pairs of `Pq Tq` plus 64 samples in zigzag order.
fn read_quantization(data: &[u8], tables: &mut [[u16; 64]; 4]) -> Result<(), DecodeError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let precision = data[cursor] >> 4;
        if precision != 0 {
            return Err(DecodeError::Unsupported("16-bit quantisation tables"));
        }
        let id = usize::from(data[cursor] & 0x0F);
        if id > 3 {
            return Err(DecodeError::Malformed("quantisation table id out of range"));
        }
        if cursor + 65 > data.len() {
            return Err(DecodeError::Truncated { offset: cursor });
        }
        for k in 0..64 {
            tables[id][k] = u16::from(data[cursor + 1 + k]);
        }
        cursor += 65;
    }
    Ok(())
}

/// A canonical Huffman table in the JPEG reference decoder form.
struct HuffmanTable {
    min_code: [u32; 17],
    /// Largest code of each length; `-1` marks a length no symbol uses.
    max_code: [i32; 17],
    val_ptr: [usize; 17],
    symbols: Vec<u8>,
}

impl HuffmanTable {
    fn build(counts: &[u8; 16], symbols: &[u8]) -> Result<Self, DecodeError> {
        let total: usize = counts.iter().map(|c| usize::from(*c)).sum();
        if total != symbols.len() || total > 256 {
            return Err(DecodeError::Malformed("Huffman symbol count disagrees with its code lengths"));
        }
        let mut min_code = [0u32; 17];
        let mut max_code = [-1i32; 17];
        let mut val_ptr = [0usize; 17];
        let mut code: i32 = 0;
        let mut index = 0usize;
        let mut weight = 0usize;
        for k in 1..=16 {
            let count = i32::from(counts[k - 1]);
            min_code[k] = code as u32;
            val_ptr[k] = index;
            max_code[k] = code + count - 1;
            index += count as usize;
            weight += (count as usize) << (16 - k);
            code = (code + count) << 1;
        }
        // JPEG forbids the all-ones code, so a complete table weighs 65535
        // rather than a full 16-bit tree.
        if weight > 65536 {
            return Err(DecodeError::Malformed("Huffman table overflows the 16-bit code space"));
        }
        Ok(Self { min_code, max_code, val_ptr, symbols: symbols.to_vec() })
    }
}

/// Reads `DHT` payload: `class/id`, 16 counts, then the symbols.
fn read_huffman(data: &[u8], dc: &mut [Option<HuffmanTable>; 4], ac: &mut [Option<HuffmanTable>; 4]) -> Result<(), DecodeError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header = data[cursor];
        let class = (header >> 4) as usize;
        let id = (header & 0x0F) as usize;
        if class > 1 || id > 3 {
            return Err(DecodeError::Malformed("Huffman table class or id out of range"));
        }
        if cursor + 17 > data.len() {
            return Err(DecodeError::Truncated { offset: cursor });
        }
        let mut counts = [0u8; 16];
        counts.copy_from_slice(&data[cursor + 1..cursor + 17]);
        let total: usize = counts.iter().map(|c| usize::from(*c)).sum();
        let end = cursor + 17 + total;
        if end > data.len() {
            return Err(DecodeError::Truncated { offset: cursor });
        }
        let table = HuffmanTable::build(&counts, &data[cursor + 17..end])?;
        if class == 0 {
            dc[id] = Some(table);
        }
        else {
            ac[id] = Some(table);
        }
        cursor = end;
    }
    Ok(())
}

/// Bit-level reader over entropy-coded data with byte unstuffing.
struct ScanReader<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    bit: i8,
    marker: Option<u8>,
}

impl<'a> ScanReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, byte: 0, bit: -1, marker: None }
    }

    /// Loads the next byte, honouring `FF00` stuffing and marker detection.
    fn ensure_byte(&mut self) -> Result<(), DecodeError> {
        if self.bit >= 0 {
            return Ok(());
        }
        if self.pos >= self.data.len() {
            return Err(DecodeError::Truncated { offset: self.pos });
        }
        let first = self.data[self.pos];
        if first == 0xFF {
            if self.pos + 1 >= self.data.len() {
                return Err(DecodeError::Truncated { offset: self.pos });
            }
            let next = self.data[self.pos + 1];
            if next == 0 {
                self.byte = 0xFF;
                self.pos += 2;
            }
            else {
                self.marker = Some(next);
                self.pos += 2;
                return Ok(());
            }
        }
        else {
            self.byte = first;
            self.pos += 1;
        }
        self.bit = 7;
        Ok(())
    }

    fn bit(&mut self) -> Result<u32, DecodeError> {
        self.ensure_byte()?;
        if self.marker.is_some() {
            return Err(match self.marker {
                Some(EOI) => DecodeError::Truncated { offset: self.pos },
                Some(m) if (0xD0..=0xD7).contains(&m) => DecodeError::Malformed("restart marker inside an MCU"),
                _ => DecodeError::Malformed("scan ended inside an MCU"),
            });
        }
        let value = u32::from((self.byte >> self.bit) & 1);
        self.bit -= 1;
        Ok(value)
    }

    fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    /// Decodes one Huffman symbol.
    fn symbol(&mut self, table: &HuffmanTable) -> Result<u8, DecodeError> {
        let mut code = self.bit()?;
        for length in 1..=16usize {
            if code as i32 <= table.max_code[length] {
                let offset = usize::try_from(code - table.min_code[length]).map_err(|_| DecodeError::Malformed("negative Huffman offset"))?;
                let index = table.val_ptr[length] + offset;
                return table.symbols.get(index).copied().ok_or(DecodeError::Malformed("Huffman symbol index out of range"));
            }
            code = (code << 1) | self.bit()?;
        }
        Err(DecodeError::Malformed("Huffman code longer than 16 bits"))
    }

    /// Reads an `n`-bit sample and applies the JPEG extension (T.81 F.3.3.7).
    fn extend(&mut self, n: u32) -> Result<i32, DecodeError> {
        if n == 0 {
            return Ok(0);
        }
        let value = self.bits(n)?;
        Ok(if value < (1 << (n - 1)) { value as i32 + 1 - (1 << n) } else { value as i32 })
    }
}

/// Decodes the entropy-coded segment; returns the reader and whether the
/// caller should keep parsing segments.
fn decode_scan<'a>(
    mut reader: ScanReader<'a>,
    frame: &mut Frame,
    scan: &Scan,
    quant: &[[u16; 64]; 4],
    dc_tables: &[Option<HuffmanTable>; 4],
    ac_tables: &[Option<HuffmanTable>; 4],
) -> Result<ScanReader<'a>, DecodeError> {
    let cos = cosine_table();
    let mcu_w = 8 * frame.max_h;
    let mcu_h = 8 * frame.max_v;
    let mcus_x = (frame.width as usize).div_ceil(mcu_w);
    let mcus_y = (frame.height as usize).div_ceil(mcu_h);
    let total = mcus_x * mcus_y;

    let mut planes: Vec<Vec<u8>> = frame.components.iter().map(|c| c.plane.clone()).collect();
    let mut predictors = vec![0i32; frame.components.len()];
    let mut done = false;

    for index in 0..total {
        if reader.marker.is_some() {
            match reader.marker {
                Some(m) if (0xD0..=0xD7).contains(&m) => {
                    reader.marker = None;
                    predictors.fill(0);
                }
                Some(EOI) => {
                    done = true;
                    break;
                }
                _ => return Err(DecodeError::Malformed("unexpected marker inside the scan")),
            }
        }
        let _ = index;
        for entry in &scan.order {
            let ci = entry.component;
            let component = &frame.components[ci];
            let dc = dc_tables[entry.dc].as_ref().ok_or(DecodeError::Malformed("scan references a missing DC Huffman table"))?;
            let ac = ac_tables[entry.ac].as_ref().ok_or(DecodeError::Malformed("scan references a missing AC Huffman table"))?;
            for by in 0..component.v {
                for bx in 0..component.h {
                    let block_col = (index % mcus_x) * component.h + bx;
                    let block_row = (index / mcus_x) * component.v + by;
                    decode_block(&mut reader, dc, ac, &quant[component.qt], &mut predictors[ci], |coeffs| {
                        let mut out = [0i32; 64];
                        idct(coeffs, &cos, &mut out);
                        write_block(&mut planes[ci], component, block_col, block_row, &out);
                    })?;
                }
            }
        }
    }

    if !done {
        // Drop the padding bits of the current byte, then consume any
        // remaining entropy bytes until the terminating marker.
        reader.bit = -1;
        while reader.marker.is_none() {
            reader.ensure_byte()?;
        }
        match reader.marker {
            Some(EOI) => {}
            Some(m) if (0xD0..=0xD7).contains(&m) => {}
            _ => return Err(DecodeError::Malformed("scan not terminated by EOI")),
        }
    }

    for (plane, component) in planes.into_iter().zip(frame.components.iter_mut()) {
        component.plane = plane;
    }
    Ok(reader)
}

/// Decodes one 8x8 block and hands the dequantised coefficients to `emit`.
fn decode_block(
    reader: &mut ScanReader<'_>,
    dc: &HuffmanTable,
    ac: &HuffmanTable,
    quant: &[u16; 64],
    predictor: &mut i32,
    emit: impl FnOnce(&[i32; 64]),
) -> Result<(), DecodeError> {
    let mut coeffs = [0i32; 64];
    let size = reader.symbol(dc)?;
    let diff = reader.extend(u32::from(size))?;
    *predictor += diff;
    coeffs[0] = *predictor * i32::from(quant[0]);

    let mut position = 1usize;
    while position < 64 {
        let rs = reader.symbol(ac)?;
        let run = (rs >> 4) as usize;
        let size = (rs & 0x0F) as u32;
        if size == 0 {
            if run == 15 {
                position += 16;
                continue;
            }
            break;
        }
        position += run;
        if position >= 64 {
            return Err(DecodeError::Malformed("AC run past the end of the block"));
        }
        let value = reader.extend(size)?;
        coeffs[ZIGZAG[position]] = value * i32::from(quant[position]);
        position += 1;
    }
    emit(&coeffs);
    Ok(())
}

/// Writes an IDCT'd block into a component plane, clipped to plane bounds.
fn write_block(plane: &mut [u8], component: &Component, block_col: usize, block_row: usize, values: &[i32; 64]) {
    let x0 = block_col * 8;
    let y0 = block_row * 8;
    for dy in 0..8 {
        let y = y0 + dy;
        if y >= component.ph {
            break;
        }
        for dx in 0..8 {
            let x = x0 + dx;
            if x >= component.pw {
                break;
            }
            plane[y * component.pw + x] = values[dy * 8 + dx].clamp(0, 255) as u8;
        }
    }
}

/// The 8x8 cosine table: `COS[k * 8 + n] = cos((2n + 1) * k * PI / 16)`.
fn cosine_table() -> [f32; 64] {
    let mut table = [0f32; 64];
    for k in 0..8 {
        for n in 0..8 {
            table[k * 8 + n] = ((2 * n + 1) as f32 * k as f32 * std::f32::consts::PI / 16.0).cos();
        }
    }
    table
}

/// Separable float IDCT with the T.81 normalisation and level shift.
fn idct(block: &[i32; 64], cos: &[f32; 64], out: &mut [i32; 64]) {
    let scale = |k: usize| if k == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
    let mut transposed = [0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0f32;
            for u in 0..8 {
                sum += scale(u) * block[y * 8 + u] as f32 * cos[u * 8 + x];
            }
            transposed[x * 8 + y] = sum;
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0f32;
            for v in 0..8 {
                sum += scale(v) * transposed[x * 8 + v] * cos[v * 8 + y];
            }
            out[y * 8 + x] = (sum * 0.25).round() as i32 + 128;
        }
    }
}

impl Frame {
    /// Turns the decoded planes into an RGBA8 bitmap, upsampling chroma by
    /// nearest neighbour and converting YCbCr (or grey) to RGB.
    fn compose(&mut self) -> Result<Bitmap, DecodeError> {
        let width = self.width as usize;
        let height = self.height as usize;
        let mut rgba = vec![0u8; width * height * 4];
        if self.components.len() == 1 {
            let luma = &self.components[0].plane;
            for y in 0..height {
                for x in 0..width {
                    let value = luma[y * width + x];
                    let d = (y * width + x) * 4;
                    rgba[d..d + 4].copy_from_slice(&[value, value, value, 255]);
                }
            }
        }
        else {
            if self.components.len() != 3 {
                return Err(DecodeError::Malformed("three components expected"));
            }
            let (luma, blue, red) = (&self.components[0], &self.components[1], &self.components[2]);
            for y in 0..height {
                for x in 0..width {
                    let y_value = i32::from(luma.plane[y * width + x]);
                    let cx = x * blue.h / self.max_h;
                    let cy = y * blue.v / self.max_v;
                    let cb = i32::from(blue.plane[cy * blue.pw + cx]) - 128;
                    let cr = i32::from(red.plane[cy * red.pw + cx]) - 128;
                    let r = y_value as f32 + 1.402 * cr as f32;
                    let g = y_value as f32 - 0.344_136 * cb as f32 - 0.714_136 * cr as f32;
                    let b = y_value as f32 + 1.772 * cb as f32;
                    let d = (y * width + x) * 4;
                    rgba[d] = r.round().clamp(0.0, 255.0) as u8;
                    rgba[d + 1] = g.round().clamp(0.0, 255.0) as u8;
                    rgba[d + 2] = b.round().clamp(0.0, 255.0) as u8;
                    rgba[d + 3] = 255;
                }
            }
        }
        Ok(Bitmap::new(self.width, self.height, rgba))
    }
}

const fn be16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

/// Zigzag index -> raster position inside an 8x8 block.
const ZIGZAG: [usize; 64] = build_zigzag();

const fn build_zigzag() -> [usize; 64] {
    let mut out = [0usize; 64];
    let mut i = 0;
    let mut diagonal = 0usize;
    while diagonal <= 14 {
        let lo = if diagonal > 7 { diagonal - 7 } else { 0 };
        let hi = if diagonal < 8 { diagonal } else { 7 };
        let mut step = 0;
        while step <= hi - lo {
            let u = if diagonal % 2 == 0 { lo + step } else { hi - step };
            let v = diagonal - u;
            out[i] = v * 8 + u;
            i += 1;
            step += 1;
        }
        diagonal += 1;
    }
    out
}
