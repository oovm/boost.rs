//! Baseline sequential JPEG encoder (ITU-T T.81), written from the
//! specification with no third-party codec.
//!
//! The input is NV12 — the same sample format the video pipeline already
//! produces — which maps onto JPEG's 4:2:0 layout directly: the Y plane is
//! the luma component and the interleaved UV plane splits into the two
//! half-resolution chroma components. This is the video codec of the
//! portable MP4 backend on platforms without a system H.264 encoder.

use media_types::Error;

/// Annex K.1.1 luminance quantisation table, already in zig-zag order.
const LUMA_QUANT: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 18, 25, 52, 64, 68, 75, 26, 27, 30,
    44, 67, 83, 81, 87, 65, 70, 78, 75, 87, 78, 89, 93, 92, 84, 89, 93, 124, 117, 135, 148, 119, 128, 135, 138, 140, 161, 161, 120,
];

/// Annex K.1.2 chrominance quantisation table, zig-zag order.
const CHROMA_QUANT: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// Annex K.3.3.1 DC luminance: `bits[i]` is the count of codes of length `i`.
const DC_LUMA_BITS: [u8; 17] = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
/// Annex K.3.3.2 DC chrominance.
const DC_CHROMA_BITS: [u8; 17] = [0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
/// Annex K.3.4.1 AC luminance.
const AC_LUMA_BITS: [u8; 17] = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
/// Annex K.3.4.2 AC chrominance.
const AC_CHROMA_BITS: [u8; 17] = [0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];

const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Annex K.3.4.1 AC luminance symbol values (`RRRRSSSS` pairs).
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1,
    0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
    0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85,
    0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa,
    0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
    0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9,
    0xfa,
];

/// Annex K.3.4.2 AC chrominance symbol values.
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42,
    0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19,
    0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55,
    0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83,
    0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8,
    0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4,
    0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9,
    0xfa,
];

/// Zig-zag scan: entry `k` is the raster index of the `k`-th coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57,
    50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The encoder's default picture quality (1..=99).
pub const DEFAULT_QUALITY: u8 = 85;

/// Scales an Annex K table by `quality` with the standard JPEG formula.
fn quant_table(base: &[u8; 64], quality: u8) -> [u16; 64] {
    let q = i32::from(quality.clamp(1, 99));
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut table = [0u16; 64];
    for (entry, value) in table.iter_mut().zip(base) {
        *entry = ((i32::from(*value) * scale + 50) / 100).clamp(1, 255) as u16;
    }
    table
}

/// A canonical Huffman table: `(code, length)` per symbol value.
/// Unused symbols keep `(0, 0)` and must never be encoded.
struct Huffman {
    entries: [(u16, u8); 256],
}

impl Huffman {
    fn build(bits: &[u8; 17], values: &[u8]) -> Self {
        let mut entries = [(0u16, 0u8); 256];
        let mut code = 0u16;
        let mut index = 0usize;
        for (length, &count) in bits.iter().enumerate().skip(1) {
            for _ in 0..count {
                entries[usize::from(values[index])] = (code, length as u8);
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        Self { entries }
    }
}

/// Entropy-coded segment writer with the mandatory `0xFF` byte stuffing.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    current: u8,
    filled: u32,
}

impl BitWriter<'_> {
    fn write_bits(&mut self, value: u16, count: u32) {
        for bit in (0..count).rev() {
            self.current = (self.current << 1) | (((value >> bit) & 1) as u8);
            self.filled += 1;
            if self.filled == 8 {
                let byte = self.current;
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0x00);
                }
                self.current = 0;
                self.filled = 0;
            }
        }
    }

    fn write_symbol(&mut self, table: &Huffman, symbol: u8) {
        let (code, length) = table.entries[usize::from(symbol)];
        self.write_bits(code, u32::from(length));
    }

    /// Pads the final partial byte with 1 bits, as T.81 B.1.1.3 requires.
    fn finish(mut self) {
        if self.filled > 0 {
            let pad = 8 - self.filled;
            self.current |= (1 << pad) - 1;
            let byte = self.current;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
        }
    }
}

/// Number of bits needed to represent `value` (the JPEG `SSSS` size class).
const fn bit_size(value: i32) -> u8 {
    32 - value.unsigned_abs().leading_zeros() as u8
}

/// Separable float DCT-II of one level-shifted 8x8 block, quantised into
/// zig-zag order. The cosine table is `cos[(2x+1)u*pi/16]`.
fn dct_block(samples: &[f32; 64], quant: &[u16; 64], cos: &[[f32; 8]; 8]) -> [i32; 64] {
    let mut freq = [0f32; 64];
    // Rows: tmp[u][y] = sum_x samples[y][x] * cos[u][x].
    let mut tmp = [0f32; 64];
    for y in 0..8 {
        for u in 0..8 {
            let mut sum = 0f32;
            for x in 0..8 {
                sum += samples[y * 8 + x] * cos[u][x];
            }
            tmp[u * 8 + y] = sum;
        }
    }
    // Columns, with the 1/4 and C(0)=1/sqrt(2) normalisation folded in.
    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0f32;
            for y in 0..8 {
                sum += tmp[u * 8 + y] * cos[v][y];
            }
            let mut scale = 0.25;
            if u == 0 {
                scale *= core::f32::consts::FRAC_1_SQRT_2;
            }
            if v == 0 {
                scale *= core::f32::consts::FRAC_1_SQRT_2;
            }
            // Stored row-major by (vertical, horizontal) frequency so the
            // zig-zag raster indices address it directly.
            freq[v * 8 + u] = sum * scale;
        }
    }
    let mut out = [0i32; 64];
    for (k, out_entry) in out.iter_mut().enumerate() {
        let raster = ZIGZAG[k];
        *out_entry = (freq[raster] / f32::from(quant[k])).round() as i32;
    }
    out
}

/// Extracts one 8x8 block from a plane, clamping at the right/bottom edge
/// (edge replication, the standard padding for partial MCUs).
fn take_block(plane: &[u8], width: usize, height: usize, bx: usize, by: usize) -> [f32; 64] {
    let mut samples = [0f32; 64];
    for j in 0..8 {
        let y = (by * 8 + j).min(height - 1);
        for i in 0..8 {
            let x = (bx * 8 + i).min(width - 1);
            samples[j * 8 + i] = f32::from(plane[y * width + x]) - 128.0;
        }
    }
    samples
}

/// Entropy-codes one block, returning the updated DC predictor.
fn encode_block(
    plane: &[u8],
    width: usize,
    height: usize,
    bx: usize,
    by: usize,
    quant: &[u16; 64],
    dc: &Huffman,
    ac: &Huffman,
    prev_dc: i32,
    cos: &[[f32; 8]; 8],
    writer: &mut BitWriter<'_>,
) -> i32 {
    let coefficients = dct_block(&take_block(plane, width, height, bx, by), quant, cos);
    let dc_value = coefficients[0];
    let diff = dc_value - prev_dc;
    let size = bit_size(diff);
    writer.write_symbol(dc, size);
    if size > 0 {
        let bits = if diff < 0 { diff - 1 } else { diff };
        writer.write_bits((bits as u16) & ((1 << size) - 1), u32::from(size));
    }
    let mut run = 0u8;
    for &value in &coefficients[1..] {
        if value == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            writer.write_symbol(ac, 0xF0); // zero run-length
            run -= 16;
        }
        let size = bit_size(value);
        writer.write_symbol(ac, (run << 4) | size);
        let bits = if value < 0 { value - 1 } else { value };
        writer.write_bits((bits as u16) & ((1 << size) - 1), u32::from(size));
        run = 0;
    }
    if run > 0 {
        writer.write_symbol(ac, 0x00); // end of block
    }
    dc_value
}

/// Appends `payload` as one marker segment.
fn segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    out.push(0xFF);
    out.push(marker);
    let length = (payload.len() + 2) as u16;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
}

/// Writes SOI, APP0/JFIF, DQT, SOF0, DHT and SOS ahead of the scan data.
fn write_headers(out: &mut Vec<u8>, width: usize, height: usize, luma_quant: &[u16; 64], chroma_quant: &[u16; 64]) {
    out.extend_from_slice(&[0xFF, 0xD8]); // SOI
    segment(
        out,
        0xE0,
        &[
            b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, // JFIF 1.1, no thumbnail
        ],
    );
    for (id, table) in [(0u8, luma_quant), (1, chroma_quant)] {
        let mut payload = vec![id];
        payload.extend(table.iter().map(|&value| value as u8));
        segment(out, 0xDB, &payload);
    }
    segment(
        out,
        0xC0,
        &[
            8,
            (height >> 8) as u8,
            height as u8,
            (width >> 8) as u8,
            width as u8,
            3,
            1,
            0x22,
            0, // Y, 2x2 sampling, quant table 0
            2,
            0x11,
            1, // Cb, 1x1, table 1
            3,
            0x11,
            1, // Cr, 1x1, table 1
        ],
    );
    for (class_id, bits, values) in [
        (0x00u8, &DC_LUMA_BITS, &DC_VALUES[..]),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
        (0x01, &DC_CHROMA_BITS, &DC_VALUES[..]),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
    ] {
        let mut payload = vec![class_id];
        payload.extend_from_slice(&bits[1..]);
        payload.extend_from_slice(values);
        segment(out, 0xC4, &payload);
    }
    segment(out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]); // 3 components, full baseline scan
}

/// Encodes one NV12 frame (Y plane then interleaved UV, stride = width) as a
/// baseline 4:2:0 JPEG.
///
/// `quality` is the standard 1..=99 scale; [`DEFAULT_QUALITY`] is what the
/// portable video backend uses.
pub fn encode_nv12(width: u32, height: u32, nv12: &[u8], quality: u8) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 {
        return Err(Error::invalid_input("jpeg: zero dimension"));
    }
    if width > 65_535 || height > 65_535 {
        return Err(Error::invalid_input("jpeg: baseline JPEG caps frames at 65535 pixels per side"));
    }
    if width % 2 == 1 || height % 2 == 1 {
        return Err(Error::invalid_input("jpeg: 4:2:0 sampling needs even dimensions"));
    }
    let (w, h) = (width as usize, height as usize);
    let expected = w * h + w * h / 2;
    if nv12.len() < expected {
        return Err(Error::invalid_input(format!("jpeg: NV12 buffer is {} bytes, expected {expected}", nv12.len())));
    }

    let luma_quant = quant_table(&LUMA_QUANT, quality);
    let chroma_quant = quant_table(&CHROMA_QUANT, quality);
    let mut out = Vec::with_capacity(w * h / 2);
    write_headers(&mut out, w, h, &luma_quant, &chroma_quant);

    let dc_luma = Huffman::build(&DC_LUMA_BITS, &DC_VALUES);
    let dc_chroma = Huffman::build(&DC_CHROMA_BITS, &DC_VALUES);
    let ac_luma = Huffman::build(&AC_LUMA_BITS, &AC_LUMA_VALUES);
    let ac_chroma = Huffman::build(&AC_CHROMA_BITS, &AC_CHROMA_VALUES);

    // cos[(2x+1)*u*pi/16]
    let mut cos = [[0f32; 8]; 8];
    for (u, row) in cos.iter_mut().enumerate() {
        for (x, entry) in row.iter_mut().enumerate() {
            *entry = (((2 * x + 1) * u) as f32 * std::f32::consts::PI / 16.0).cos();
        }
    }

    let y = &nv12[..w * h];
    let uv = &nv12[w * h..expected];
    let (cw, ch) = (w / 2, h / 2);
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    for (i, slot) in cb.iter_mut().enumerate() {
        *slot = uv[2 * i];
    }
    for (i, slot) in cr.iter_mut().enumerate() {
        *slot = uv[2 * i + 1];
    }

    let mut writer = BitWriter { out: &mut out, current: 0, filled: 0 };
    let mut prev = [0i32; 3];
    let mcus_x = w.div_ceil(16);
    let mcus_y = h.div_ceil(16);
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for j in 0..2 {
                for i in 0..2 {
                    prev[0] = encode_block(y, w, h, mx * 2 + i, my * 2 + j, &luma_quant, &dc_luma, &ac_luma, prev[0], &cos, &mut writer);
                }
            }
            prev[1] = encode_block(&cb, cw, ch, mx, my, &chroma_quant, &dc_chroma, &ac_chroma, prev[1], &cos, &mut writer);
            prev[2] = encode_block(&cr, cw, ch, mx, my, &chroma_quant, &dc_chroma, &ac_chroma, prev[2], &cos, &mut writer);
        }
    }
    writer.finish();
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_QUALITY, encode_nv12};

    /// A smooth NV12 frame: luma gradient, constant chroma.
    fn frame(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut nv12 = vec![0u8; w * h + w * h / 2];
        for y in 0..h {
            for x in 0..w {
                nv12[y * w + x] = ((x * 255) / w.max(1)) as u8;
            }
        }
        for byte in &mut nv12[w * h..] {
            *byte = 128;
        }
        nv12
    }

    fn marker_at(bytes: &[u8], offset: usize) -> Option<u8> {
        bytes.get(offset).copied().filter(|&b| b == 0xFF)?;
        bytes.get(offset + 1).copied()
    }

    #[test]
    fn emits_the_baseline_marker_chain() {
        let jpeg = encode_nv12(32, 32, &frame(32, 32), DEFAULT_QUALITY).expect("encode");
        assert_eq!(marker_at(&jpeg, 0), Some(0xD8), "SOI");
        assert_eq!(marker_at(&jpeg, 2), Some(0xE0), "APP0");
        // The mandatory tables appear before the scan; the last two markers
        // are SOS and EOI with only entropy data between them.
        assert!(jpeg.windows(2).any(|w| w == [0xFF, 0xC0]), "SOF0");
        assert!(jpeg.windows(2).any(|w| w == [0xFF, 0xDB]), "DQT");
        assert!(jpeg.windows(2).any(|w| w == [0xFF, 0xC4]), "DHT");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "EOI");
    }

    #[test]
    fn scans_never_emit_a_bare_ff_zero_pair() {
        // Byte stuffing guarantees 0xFF inside the scan is always followed
        // by 0x00; a raw 0xFF would be a premature marker.
        let jpeg = encode_nv12(64, 48, &frame(64, 48), 70).expect("encode");
        let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).expect("SOS");
        let eoi = jpeg.len() - 2;
        let mut at = sos + 4 + usize::from(u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]])) - 2;
        // `at` is now just past the SOS marker segment.
        while at < eoi {
            if jpeg[at] == 0xFF {
                assert_eq!(jpeg[at + 1], 0x00, "un-stuffed 0xFF in scan data");
                at += 2;
            }
            else {
                at += 1;
            }
        }
    }

    /// Independent oracle: the Windows system decoder (WIC) must read our
    /// hand-written stream back with the right geometry and gradient axis.
    #[test]
    #[cfg(windows)]
    fn the_system_image_decoder_reads_the_stream_back() {
        use crate::decode::{DecodeLimits, MediaLibrary};

        let jpeg = encode_nv12(64, 48, &frame(64, 48), DEFAULT_QUALITY).expect("encode");
        let path = std::env::temp_dir().join("media-codec-jpeg-wic.jpg");
        std::fs::write(&path, &jpeg).expect("write jpg");

        let mut library = MediaLibrary::new();
        let bitmap = library.image(&path.to_string_lossy(), DecodeLimits::project_default()).expect("WIC must decode our baseline stream");
        assert_eq!(bitmap.width, 64);
        assert_eq!(bitmap.height, 48);

        let luma = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            0.299 * f32::from(bitmap.rgba[i]) + 0.587 * f32::from(bitmap.rgba[i + 1]) + 0.114 * f32::from(bitmap.rgba[i + 2])
        };
        // The input luma ramps left→right from 0 to ~254; chroma is neutral,
        // so the decoded pixels must be grey with the same ramp. A transposed
        // DCT would come back as a top→bottom ramp instead.
        assert!(luma(2, 24) < 40.0, "left edge dark, got {}", luma(2, 24));
        assert!(luma(61, 24) > 200.0, "right edge bright, got {}", luma(61, 24));
        let vertical: f32 = (0..64).map(|x| luma(x, 47) - luma(x, 0)).sum();
        assert!(vertical.abs() < 640.0, "gradient must run across the width, vertical drift {vertical}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let error = encode_nv12(33, 32, &frame(34, 32), DEFAULT_QUALITY).expect_err("odd width must fail");
        assert_eq!(error.code(), "invalid_input");
    }

    #[test]
    fn short_buffers_are_rejected() {
        let error = encode_nv12(16, 16, &[0u8; 10], DEFAULT_QUALITY).expect_err("truncated NV12 must fail");
        assert_eq!(error.code(), "invalid_input");
    }
}
