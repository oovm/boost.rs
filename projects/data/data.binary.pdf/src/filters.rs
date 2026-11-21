//! Stream filter decoding.
//!
//! `/FlateDecode`, `/LZWDecode`, `/ASCIIHexDecode`, `/ASCII85Decode` and
//! `/RunLengthDecode` are implemented, including the `/DecodeParms` those
//! filters define. Image codecs (`/DCTDecode`, `/JBIG2Decode`,
//! `/CCITTFaxDecode`, `/JPXDecode`) stay unsupported and are reported instead
//! of approximated, so a caller can never mistake a guess for decoded data.

use std::collections::BTreeMap;

use data_compression::{CodecLimits, Zlib};

use crate::error::{Result, invalid, unsupported};
use crate::object::{Dict, PdfObject, PdfStream};

/// Upper bound for one decoded stream. A malformed document cannot force an
/// unbounded allocation.
pub const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;

/// Decodes a stream through its `/Filter` chain.
///
/// `objects` resolves indirect `/Filter` and `/DecodeParms` values. Unsupported
/// filters return `format/unsupported` without touching the original bytes.
pub fn decode(stream: &PdfStream, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<u8>> {
    let filters = filter_names(&stream.dict, objects)?;
    if filters.is_empty() {
        return Ok(stream.data.clone());
    }
    let parameters = decode_parameters(&stream.dict, objects, filters.len())?;

    let mut data = stream.data.clone();
    for (index, filter) in filters.iter().enumerate() {
        let parameters = parameters.get(index).and_then(Option::as_ref);
        match filter.as_str() {
            "FlateDecode" | "Fl" => data = decode_flate(&data, parameters, objects)?,
            "LZWDecode" | "LZW" => data = decode_lzw(&data, parameters, objects)?,
            "ASCIIHexDecode" | "AHx" => data = decode_ascii_hex(&data)?,
            "ASCII85Decode" | "A85" => data = decode_ascii85(&data)?,
            "RunLengthDecode" | "RL" => data = decode_run_length(&data)?,
            "none" => {}
            other => {
                return Err(unsupported(format!("stream filter /{other} is not supported, raw bytes are preserved")));
            }
        }
    }
    Ok(data)
}

/// The `/Filter` chain of a stream as decoded names, without decoding any data.
///
/// Image extraction needs the chain to decide whether a payload is a JPEG or a
/// PNG source; a malformed `/Filter` entry is still an error.
pub fn filter_chain(stream: &PdfStream, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<String>> {
    filter_names(&stream.dict, objects)
}

fn filter_names(dict: &Dict, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<String>> {
    let Some(value) = dict.get("Filter") else {
        return Ok(Vec::new());
    };
    let value = resolve(objects, value).ok_or_else(|| invalid("stream /Filter is an unresolvable reference"))?;
    match value {
        PdfObject::Null => Ok(Vec::new()),
        PdfObject::Name(name) if name == "none" => Ok(Vec::new()),
        PdfObject::Name(name) => Ok(vec![name.clone()]),
        PdfObject::Array(items) => {
            if items.is_empty() {
                return Ok(Vec::new());
            }
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let item =
                    resolve(objects, item).ok_or_else(|| invalid("stream /Filter entry is an unresolvable reference"))?;
                match item {
                    PdfObject::Name(name) => names.push(name.clone()),
                    PdfObject::Null => {}
                    _ => return Err(invalid("stream /Filter array holds a non-name value")),
                }
            }
            Ok(names)
        }
        _ => Err(invalid("stream /Filter is not a name or an array of names")),
    }
}

fn decode_parameters(dict: &Dict, objects: &BTreeMap<u32, PdfObject>, count: usize) -> Result<Vec<Option<Dict>>> {
    let mut parameters = vec![None; count];
    let Some(value) = dict.get("DecodeParms") else {
        return Ok(parameters);
    };
    let value = resolve(objects, value).ok_or_else(|| invalid("stream /DecodeParms is an unresolvable reference"))?;
    match value {
        PdfObject::Null => {}
        PdfObject::Dictionary(entry) => {
            if let Some(slot) = parameters.first_mut() {
                *slot = Some(entry.clone());
            }
        }
        PdfObject::Array(items) => {
            for (index, item) in items.iter().take(count).enumerate() {
                let item =
                    resolve(objects, item).ok_or_else(|| invalid("stream /DecodeParms entry is an unresolvable reference"))?;
                match item {
                    PdfObject::Null => {}
                    PdfObject::Dictionary(entry) => parameters[index] = Some(entry.clone()),
                    _ => return Err(invalid("stream /DecodeParms array holds a non-dictionary value")),
                }
            }
        }
        _ => return Err(invalid("stream /DecodeParms is not a dictionary or an array")),
    }
    Ok(parameters)
}

fn decode_flate(input: &[u8], parameters: Option<&Dict>, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<u8>> {
    let limits = CodecLimits::new().with_max_output_bytes(MAX_STREAM_BYTES);
    let facts = Zlib::decode(input, limits).map_err(|error| invalid(format!("FlateDecode failed: {error}")))?;
    apply_predictor(facts.value, parameters, objects)
}

/// `/ASCIIHexDecode`: one hex pair per byte, whitespace ignored, `>` ends the
/// data and an odd trailing digit is padded with zero.
fn decode_ascii_hex(input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 2 + 1);
    let mut high: Option<u8> = None;
    for &byte in input {
        if is_filter_whitespace(byte) {
            continue;
        }
        if byte == b'>' {
            break;
        }
        let digit = hex_digit(byte).ok_or_else(|| invalid(format!("ASCIIHexDecode found invalid byte 0x{byte:02X}")))?;
        match high.take() {
            Some(previous) => {
                output.push(previous * 16 + digit);
                ensure_output_size(output.len())?;
            }
            None => high = Some(digit),
        }
    }
    if let Some(previous) = high {
        output.push(previous * 16);
    }
    Ok(output)
}

/// `/ASCII85Decode`: five base-85 characters per four bytes, `z` for four zero
/// bytes, `~>` for the end of data.
fn decode_ascii85(input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut group = [0u8; 5];
    let mut count = 0usize;
    let mut index = 0usize;
    while index < input.len() {
        let byte = input[index];
        index += 1;
        if is_filter_whitespace(byte) {
            continue;
        }
        if byte == b'~' {
            break;
        }
        if byte == b'z' {
            if count != 0 {
                return Err(invalid("ASCII85Decode 'z' appears inside a group"));
            }
            output.extend_from_slice(&[0, 0, 0, 0]);
            ensure_output_size(output.len())?;
            continue;
        }
        if !(b'!'..=b'u').contains(&byte) {
            return Err(invalid(format!("ASCII85Decode found invalid byte 0x{byte:02X}")));
        }
        group[count] = byte - b'!';
        count += 1;
        if count == 5 {
            append_ascii85_group(&mut output, &group, 4)?;
            count = 0;
        }
    }
    if count > 0 {
        if count == 1 {
            return Err(invalid("ASCII85Decode has a one-character trailing group"));
        }
        for slot in group.iter_mut().skip(count) {
            *slot = 84;
        }
        append_ascii85_group(&mut output, &group, count - 1)?;
    }
    Ok(output)
}

fn append_ascii85_group(output: &mut Vec<u8>, group: &[u8; 5], bytes: usize) -> Result<()> {
    let value = group.iter().fold(0u64, |current, digit| current * 85 + u64::from(*digit));
    if bytes == 4 && value > u64::from(u32::MAX) {
        return Err(invalid("ASCII85Decode group overflows four bytes"));
    }
    let value = value as u32;
    output.extend_from_slice(&value.to_be_bytes()[..bytes]);
    ensure_output_size(output.len())
}

/// `/RunLengthDecode`: `0..=127` starts a literal run, `129..=255` repeats the
/// next byte, `128` ends the data.
fn decode_run_length(input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut index = 0usize;
    while let Some(&header) = input.get(index) {
        index += 1;
        match header {
            128 => return Ok(output),
            0..=127 => {
                let count = usize::from(header) + 1;
                let end = index
                    .checked_add(count)
                    .filter(|end| *end <= input.len())
                    .ok_or_else(|| invalid("RunLengthDecode literal run is truncated"))?;
                output.extend_from_slice(&input[index..end]);
                index = end;
            }
            other => {
                let byte = *input.get(index).ok_or_else(|| invalid("RunLengthDecode repeat run is truncated"))?;
                index += 1;
                output.extend(std::iter::repeat_n(byte, 257 - usize::from(other)));
            }
        }
        ensure_output_size(output.len())?;
    }
    Ok(output)
}

/// `/LZWDecode` with PDF's variable code width and `/EarlyChange` (default 1).
fn decode_lzw(input: &[u8], parameters: Option<&Dict>, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<u8>> {
    let early_change = match parameters.and_then(|dict| integer(dict, "EarlyChange", objects)).unwrap_or(1) {
        0 => 0usize,
        1 => 1usize,
        other => return Err(unsupported(format!("LZWDecode /EarlyChange {other} is not supported"))),
    };

    let mut table: Vec<Vec<u8>> = (0..=255).map(|byte| vec![byte]).collect();
    table.push(Vec::new()); // 256: clear table.
    table.push(Vec::new()); // 257: end of data.

    let mut reader = BitReader::new(input);
    let mut width = 9usize;
    let mut output = Vec::new();
    let mut previous: Option<Vec<u8>> = None;

    loop {
        let Some(code) = reader.read(width) else {
            if reader.remaining_bits_are_zero() {
                break;
            }
            return Err(invalid("LZWDecode stream ends inside a code"));
        };
        match code {
            256 => {
                table.truncate(258);
                width = 9;
                previous = None;
            }
            257 => break,
            _ => {
                let entry = if usize::from(code) < table.len() {
                    table[usize::from(code)].clone()
                } else if usize::from(code) == table.len() {
                    let previous = previous.as_ref().ok_or_else(|| invalid(format!("LZWDecode code {code} has no prefix")))?;
                    let mut entry = previous.clone();
                    entry.push(previous[0]);
                    entry
                } else {
                    return Err(invalid(format!("LZWDecode code {code} is outside the table")));
                };
                output.extend_from_slice(&entry);
                ensure_output_size(output.len())?;
                if let Some(previous) = previous.as_ref()
                    && table.len() < 4096
                {
                    let mut next = previous.clone();
                    next.push(entry[0]);
                    table.push(next);
                }
                previous = Some(entry);
                if width < 12 && table.len() + early_change >= (1 << width) {
                    width += 1;
                }
            }
        }
    }
    apply_predictor(output, parameters, objects)
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read(&mut self, width: usize) -> Option<u16> {
        if self.bit.checked_add(width)? > self.data.len().checked_mul(8)? {
            return None;
        }
        let mut value = 0u16;
        for _ in 0..width {
            let byte = self.data[self.bit / 8];
            let bit = (byte >> (7 - (self.bit % 8))) & 1;
            value = (value << 1) | u16::from(bit);
            self.bit += 1;
        }
        Some(value)
    }

    fn remaining_bits_are_zero(&self) -> bool {
        (self.bit..self.data.len() * 8).all(|position| (self.data[position / 8] >> (7 - (position % 8))) & 1 == 0)
    }
}

fn is_filter_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn ensure_output_size(length: usize) -> Result<()> {
    if length as u64 > MAX_STREAM_BYTES {
        return Err(invalid("decoded stream exceeds the 64 MiB limit"));
    }
    Ok(())
}

fn apply_predictor(data: Vec<u8>, parameters: Option<&Dict>, objects: &BTreeMap<u32, PdfObject>) -> Result<Vec<u8>> {
    let Some(parameters) = parameters else {
        return Ok(data);
    };
    let predictor = integer(parameters, "Predictor", objects).unwrap_or(1);
    if predictor <= 1 {
        return Ok(data);
    }
    let colors = integer(parameters, "Colors", objects).unwrap_or(1);
    let bits = integer(parameters, "BitsPerComponent", objects).unwrap_or(8);
    let columns = integer(parameters, "Columns", objects).unwrap_or(1);
    if colors <= 0 || bits <= 0 || columns <= 0 {
        return Err(invalid("predictor parameters must be positive"));
    }
    if bits != 8 {
        return Err(unsupported(format!("predictor with {bits} bits per component is not supported")));
    }
    let colors = usize::try_from(colors).map_err(|_| invalid("predictor /Colors is too large"))?;
    let columns = usize::try_from(columns).map_err(|_| invalid("predictor /Columns is too large"))?;
    let row_length = colors.checked_mul(columns).ok_or_else(|| invalid("predictor row is too large"))?;
    if row_length == 0 {
        return Ok(data);
    }

    match predictor {
        2 => tiff_predictor(data, colors, row_length),
        10..=15 => png_predictor(&data, colors, row_length),
        other => Err(unsupported(format!("predictor {other} is not supported"))),
    }
}

/// TIFF predictor 2: each byte is the difference from the pixel `colors` bytes
/// to its left in the same row.
fn tiff_predictor(mut data: Vec<u8>, colors: usize, row_length: usize) -> Result<Vec<u8>> {
    if data.len() % row_length != 0 {
        return Err(invalid("TIFF predictor data is not a whole number of rows"));
    }
    for row in data.chunks_mut(row_length) {
        for index in colors..row.len() {
            row[index] = row[index].wrapping_add(row[index - colors]);
        }
    }
    Ok(data)
}

fn png_predictor(data: &[u8], colors: usize, row_length: usize) -> Result<Vec<u8>> {
    let stride = row_length.checked_add(1).ok_or_else(|| invalid("predictor row is too large"))?;
    if data.len() % stride != 0 {
        return Err(invalid("PNG predictor data is not a whole number of rows"));
    }

    let total = data.len() / stride * row_length;
    let mut output = Vec::with_capacity(total);
    let mut previous = vec![0u8; row_length];
    for row in data.chunks_exact(stride) {
        let filter = row[0];
        let raw = &row[1..];
        let mut current = raw.to_vec();
        match filter {
            0 => {}
            1 => {
                for index in colors..row_length {
                    current[index] = current[index].wrapping_add(current[index - colors]);
                }
            }
            2 => {
                for index in 0..row_length {
                    current[index] = current[index].wrapping_add(previous[index]);
                }
            }
            3 => {
                for index in 0..row_length {
                    let left = if index >= colors { u16::from(current[index - colors]) } else { 0 };
                    let up = u16::from(previous[index]);
                    current[index] = current[index].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                for index in 0..row_length {
                    let left = if index >= colors { current[index - colors] } else { 0 };
                    let up = previous[index];
                    let up_left = if index >= colors { previous[index - colors] } else { 0 };
                    current[index] = current[index].wrapping_add(paeth(left, up, up_left));
                }
            }
            other => return Err(invalid(format!("PNG predictor row filter {other} is invalid"))),
        }
        output.extend_from_slice(&current);
        previous = current;
    }
    Ok(output)
}

fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let left = i32::from(left);
    let up = i32::from(up);
    let up_left = i32::from(up_left);
    let estimate = left + up - up_left;
    let left_distance = (estimate - left).abs();
    let up_distance = (estimate - up).abs();
    let corner_distance = (estimate - up_left).abs();
    if left_distance <= up_distance && left_distance <= corner_distance {
        left as u8
    } else if up_distance <= corner_distance {
        up as u8
    } else {
        up_left as u8
    }
}

fn integer(dict: &Dict, key: &str, objects: &BTreeMap<u32, PdfObject>) -> Option<i64> {
    let value = dict.get(key)?;
    match resolve(objects, value)? {
        PdfObject::Integer(value) => Some(*value),
        _ => None,
    }
}

fn resolve<'a>(objects: &'a BTreeMap<u32, PdfObject>, value: &'a PdfObject) -> Option<&'a PdfObject> {
    let mut current = value;
    for _ in 0..16 {
        match current {
            PdfObject::Reference(number, _) => current = objects.get(number)?,
            _ => return Some(current),
        }
    }
    None
}
