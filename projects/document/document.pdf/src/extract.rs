use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use media_codec::PngColor;

use crate::ExtractedImage;
use crate::error::{Result, invalid, write_error_at};
use crate::filters;
use crate::object::{Dict, PdfObject, PdfStream};
use crate::parse::{PdfPage, PdfDocument, Parser, decode_text_string, is_delimiter, is_whitespace, resolve_object};

pub(crate) struct Extraction {
    pub markdown: String,
    pub losses: Vec<Loss>,
}

pub(crate) struct Loss {
    pub code: String,
    pub page: usize,
    pub message: String,
}

/// Extracts visible text from content streams decoded through their filters.
///
/// `/FlateDecode` is decoded in full. Every other filter is reported as a loss
/// and keeps its raw bytes, which is why the capability matrix still calls text
/// extraction partial rather than supported.
pub(crate) fn extract(document: &PdfDocument) -> Result<Extraction> {
    let mut pages = Vec::new();
    let mut losses = Vec::new();
    for (index, page) in document.pages.iter().enumerate() {
        let number = index.saturating_add(1);
        let streams = contents_of(document, page)?;
        let mut lines = Vec::new();
        for stream in streams {
            match filters::decode(stream, &document.objects) {
                Ok(bytes) => extract_stream(&bytes, &mut lines),
                Err(error) if error.code() == "format/unsupported" => {
                    losses.push(Loss {
                        code: "content/filter-unsupported".to_string(),
                        page: number,
                        message: error.message().to_string(),
                    });
                }
                Err(error) => {
                    losses.push(Loss {
                        code: "content/filter-invalid".to_string(),
                        page: number,
                        message: error.message().to_string(),
                    });
                }
            }
        }
        let text = lines.join("\n");
        if !text.is_empty() {
            pages.push(text);
        }
    }
    Ok(Extraction { markdown: pages.join("\n\n"), losses })
}

fn contents_of<'a>(document: &'a PdfDocument, page: &'a PdfPage) -> Result<Vec<&'a crate::object::PdfStream>> {
    let Some(value) = page.own.get("Contents") else {
        return Ok(Vec::new());
    };
    let mut streams = Vec::new();
    match resolve_object(&document.objects, value)? {
        PdfObject::Null => {}
        PdfObject::Stream(stream) => streams.push(stream),
        PdfObject::Array(items) => {
            for item in items {
                match resolve_object(&document.objects, item)? {
                    PdfObject::Stream(stream) => streams.push(stream),
                    PdfObject::Null => {}
                    _ => return Err(invalid("/Contents array holds a non-stream object")),
                }
            }
        }
        _ => return Err(invalid("/Contents is not a stream or an array of streams")),
    }
    Ok(streams)
}

enum Token {
    Object(PdfObject),
    Operator(String),
}

fn next_token(parser: &mut Parser<'_>) -> Option<Result<Token>> {
    parser.skip_ws();
    let byte = parser.peek()?;
    let object_start = byte == b'/'
        || byte == b'('
        || byte == b'<'
        || byte == b'['
        || byte == b'+'
        || byte == b'-'
        || byte == b'.'
        || byte.is_ascii_digit();
    if object_start {
        return Some(parser.parse_object(0).map(Token::Object));
    }

    let start = parser.pos;
    while let Some(current) = parser.peek() {
        if is_whitespace(current) || is_delimiter(current) {
            break;
        }
        parser.bump();
    }
    if parser.pos == start {
        parser.bump();
        return next_token(parser);
    }
    let token = parser.data.get(start..parser.pos)?;
    Some(Ok(Token::Operator(String::from_utf8_lossy(token).into_owned())))
}

fn extract_stream(bytes: &[u8], lines: &mut Vec<String>) {
    let mut parser = Parser::new(bytes);
    let mut operands: Vec<PdfObject> = Vec::new();
    let mut line = String::new();

    while let Some(token) = next_token(&mut parser) {
        let Ok(token) = token else {
            break;
        };
        match token {
            Token::Object(object) => operands.push(object),
            Token::Operator(operator) => {
                match operator.as_str() {
                    "Tj" => {
                        if let Some(PdfObject::String(value)) = operands.last() {
                            line.push_str(&decode_text_string(value));
                        }
                    }
                    "TJ" => {
                        if let Some(PdfObject::Array(items)) = operands.last() {
                            for item in items {
                                match item {
                                    PdfObject::String(value) => line.push_str(&decode_text_string(value)),
                                    PdfObject::Integer(offset) if *offset <= -100 => push_word_gap(&mut line),
                                    PdfObject::Real(offset) if *offset <= -100.0 => push_word_gap(&mut line),
                                    _ => {}
                                }
                            }
                        }
                    }
                    "'" => {
                        if let Some(PdfObject::String(value)) = operands.last() {
                            line.push_str(&decode_text_string(value));
                        }
                        push_line(lines, &mut line);
                    }
                    "\"" => {
                        if let Some(PdfObject::String(value)) = operands.last() {
                            line.push_str(&decode_text_string(value));
                        }
                        push_line(lines, &mut line);
                    }
                    "Td" | "TD" | "T*" | "Tm" => push_line(lines, &mut line),
                    "ET" => push_line(lines, &mut line),
                    "BI" => skip_inline_image(&mut parser),
                    _ => {}
                }
                operands.clear();
            }
        }
    }
    push_line(lines, &mut line);
}

fn push_word_gap(line: &mut String) {
    if !line.is_empty() && !line.ends_with(' ') {
        line.push(' ');
    }
}

fn push_line(lines: &mut Vec<String>, line: &mut String) {
    let text = line.trim_end().to_string();
    if !text.is_empty() {
        lines.push(text);
    }
    line.clear();
}

fn skip_inline_image(parser: &mut Parser<'_>) {
    while let Some(byte) = parser.peek() {
        if byte == b'E'
            && parser.data.get(parser.pos + 1) == Some(&b'I')
            && parser
                .data
                .get(parser.pos.wrapping_sub(1))
                .copied()
                .map(|previous| is_whitespace(previous) || is_delimiter(previous))
                .unwrap_or(true)
        {
            parser.pos = parser.pos.saturating_add(2).min(parser.data.len());
            return;
        }
        parser.bump();
    }
}

/// Deepest `/Resources` nesting followed inside Form XObjects.
const MAX_FORM_DEPTH: usize = 8;
/// Upper bound for one raw image payload, matching the stream decode limit.
const MAX_IMAGE_BYTES: u64 = filters::MAX_STREAM_BYTES;

struct ImageCollector<'a> {
    document: &'a PdfDocument,
    out_dir: &'a Path,
    stem: &'a str,
    page: usize,
    next_index: usize,
    used: HashSet<String>,
    images: Vec<ExtractedImage>,
}

/// Walks every page's `/Resources /XObject` graph and writes each image whose
/// filter chain this engine can decode.
///
/// 8-bit DeviceGray, DeviceRGB, DeviceCMYK and Indexed images become PNG;
/// `/Decode` ranges are honoured where the PDF spec requires an inversion.
/// Images that cannot be decoded are returned with an empty `path` and a
/// `loss` explanation; a malformed resource entry is skipped rather than
/// failing the whole document.
pub(crate) fn extract_images(document: &PdfDocument, out_dir: &Path, stem: &str) -> Result<Vec<ExtractedImage>> {
    fs::create_dir_all(out_dir).map_err(|error| write_error_at(out_dir, format!("cannot create {}: {error}", out_dir.display())))?;

    let mut images = Vec::new();
    for (index, page) in document.pages.iter().enumerate() {
        let mut collector = ImageCollector {
            document,
            out_dir,
            stem,
            page: index + 1,
            next_index: 1,
            used: HashSet::new(),
            images: Vec::new(),
        };
        if let Some(resources) = page.inherited.get("Resources") {
            let mut visited = HashSet::new();
            walk_xobjects(document, resources, 0, &mut visited, &mut collector)?;
        }
        images.append(&mut collector.images);
    }
    Ok(images)
}

fn walk_xobjects(
    document: &PdfDocument,
    resources: &PdfObject,
    depth: usize,
    visited: &mut HashSet<u32>,
    collector: &mut ImageCollector<'_>,
) -> Result<()> {
    if depth > MAX_FORM_DEPTH {
        return Ok(());
    }
    let Some(resources) = resolve_object(&document.objects, resources).ok().and_then(PdfObject::as_dict) else {
        return Ok(());
    };
    let Some(xobjects) = resources.get("XObject") else {
        return Ok(());
    };
    let Some(xobjects) = resolve_object(&document.objects, xobjects).ok().and_then(PdfObject::as_dict) else {
        return Ok(());
    };

    for (name, value) in xobjects {
        let Ok(PdfObject::Stream(stream)) = resolve_object(&document.objects, value) else {
            continue;
        };
        match stream.dict.get("Subtype").and_then(PdfObject::as_name) {
            Some("Image") => handle_image(document, name, stream, collector)?,
            Some("Form") => {
                if let PdfObject::Reference(number, _) = value
                    && !visited.insert(*number)
                {
                    continue;
                }
                if let Some(inner) = stream.dict.get("Resources") {
                    walk_xobjects(document, inner, depth + 1, visited, collector)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn handle_image(document: &PdfDocument, name: &str, stream: &PdfStream, collector: &mut ImageCollector<'_>) -> Result<()> {
    let index = collector.next_index;
    collector.next_index += 1;

    let filters = match filters::filter_chain(stream, &document.objects) {
        Ok(filters) => filters,
        Err(error) => {
            collector.push_loss(index, name, stream, "unknown", error.message());
            return Ok(());
        }
    };
    let filter = if filters.is_empty() { "none".to_string() } else { filters.join("+") };

    let width = image_number(document, &stream.dict, "Width");
    let height = image_number(document, &stream.dict, "Height");
    let (Some(width), Some(height)) = (width, height) else {
        collector.push_loss(index, name, stream, &filter, "image has no usable /Width or /Height");
        return Ok(());
    };

    if filters.iter().all(|value| value == "DCTDecode" || value == "DCT") && filters.len() == 1 {
        if !stream.data.starts_with(&[0xFF, 0xD8]) {
            collector.push_loss(index, name, stream, &filter, "DCTDecode payload does not start with the JPEG SOI marker");
            return Ok(());
        }
        let path = collector.unique_path(index, "jpg");
        fs::write(&path, &stream.data).map_err(|error| write_error_at(&path, format!("cannot write {}: {error}", path.display())))?;
        collector.images.push(ExtractedImage {
            page: collector.page,
            index,
            name: name.to_string(),
            width,
            height,
            filter,
            path: path.display().to_string(),
            loss: None,
        });
        return Ok(());
    }

    if !filters.iter().all(|value| value == "FlateDecode" || value == "Fl") {
        let message = format!("/{filter} image data cannot be written as a PNG");
        collector.push_loss(index, name, stream, &filter, &message);
        return Ok(());
    }

    let decoded = match filters::decode(stream, &document.objects) {
        Ok(decoded) => decoded,
        Err(error) => {
            collector.push_loss(index, name, stream, &filter, error.message());
            return Ok(());
        }
    };

    let color = match image_color(document, &stream.dict) {
        Ok(color) => color,
        Err(message) => {
            collector.push_loss(index, name, stream, &filter, &message);
            return Ok(());
        }
    };
    let channels = color.channels();
    if image_number(document, &stream.dict, "BitsPerComponent").unwrap_or(8) != 8 {
        collector.push_loss(index, name, stream, &filter, "only 8 bits per component images are supported");
        return Ok(());
    }

    let Some(expected) = expected_bytes(width, height, channels) else {
        collector.push_loss(index, name, stream, &filter, "image dimensions exceed the supported size");
        return Ok(());
    };
    if decoded.len() < expected {
        collector.push_loss(index, name, stream, &filter, "decoded image data is shorter than its dimensions require");
        return Ok(());
    }
    let pixels = &decoded[..expected];

    let mut losses = Vec::new();
    let alpha = match stream.dict.get("SMask") {
        Some(value) => match smask_alpha(document, value, width, height) {
            Ok(alpha) => Some(alpha),
            Err(message) => {
                losses.push(message);
                None
            }
        },
        None => None,
    };

    let converted = match color {
        ImageColor::Gray => PngPixels::Gray(pixels.to_vec()),
        ImageColor::Rgb => PngPixels::Rgb(pixels.to_vec()),
        ImageColor::Cmyk => match cmyk_to_rgb(document, &stream.dict, pixels) {
            Ok(rgb) => PngPixels::Rgb(rgb),
            Err(message) => {
                collector.push_loss(index, name, stream, &filter, message);
                return Ok(());
            }
        },
        ImageColor::Indexed(space) => match indexed_to_rgb(document, &stream.dict, &space, pixels) {
            Ok(rgb) => PngPixels::Rgb(rgb),
            Err(message) => {
                collector.push_loss(index, name, stream, &filter, message);
                return Ok(());
            }
        },
    };

    let encoded = match (converted, alpha) {
        (PngPixels::Gray(pixels), None) => media_codec::encode_png_with_color(width, height, PngColor::Gray, &pixels),
        (PngPixels::Rgb(pixels), None) => media_codec::encode_png_with_color(width, height, PngColor::Rgb, &pixels),
        (PngPixels::Gray(pixels), Some(alpha)) => {
            let mut gray_alpha = Vec::with_capacity(pixels.len() * 2);
            for (pixel, value) in pixels.iter().zip(alpha.iter()) {
                gray_alpha.extend_from_slice(&[*pixel, *value]);
            }
            media_codec::encode_png_with_color(width, height, PngColor::GrayAlpha, &gray_alpha)
        }
        (PngPixels::Rgb(pixels), Some(alpha)) => {
            let mut rgba = Vec::with_capacity(pixels.len() / 3 * 4);
            for (pixel, value) in pixels.chunks_exact(3).zip(alpha.iter()) {
                rgba.extend_from_slice(pixel);
                rgba.push(*value);
            }
            media_codec::encode_png_with_color(width, height, PngColor::Rgba, &rgba)
        }
    };
    let png = match encoded {
        Ok(png) => png,
        Err(error) => {
            collector.push_loss(index, name, stream, &filter, error.to_string());
            return Ok(());
        }
    };

    let path = collector.unique_path(index, "png");
    fs::write(&path, &png).map_err(|error| write_error_at(&path, format!("cannot write {}: {error}", path.display())))?;
    collector.images.push(ExtractedImage {
        page: collector.page,
        index,
        name: name.to_string(),
        width,
        height,
        filter,
        path: path.display().to_string(),
        loss: if losses.is_empty() { None } else { Some(losses.join("; ")) },
    });
    Ok(())
}

fn image_number(document: &PdfDocument, dict: &Dict, key: &str) -> Option<u32> {
    let value = dict.get(key)?;
    let value = match resolve_object(&document.objects, value).ok()? {
        PdfObject::Integer(value) => *value,
        _ => return None,
    };
    u32::try_from(value).ok().filter(|value| *value > 0)
}

/// Color spaces this PNG writer can encode.
enum ImageColor {
    Gray,
    Rgb,
    Cmyk,
    Indexed(IndexedSpace),
}

impl ImageColor {
    fn channels(&self) -> usize {
        match self {
            Self::Gray | Self::Indexed(_) => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
        }
    }
}

/// Palette lookup for `/Indexed` images.
struct IndexedSpace {
    /// Components per palette entry: `1` for gray, `3` for RGB.
    base: usize,
    /// Highest valid palette index.
    hival: usize,
    /// Palette data, `(hival + 1) * base` bytes at minimum.
    lookup: Vec<u8>,
}

/// Pixels ready for PNG output.
enum PngPixels {
    Gray(Vec<u8>),
    Rgb(Vec<u8>),
}

/// Resolves an image `/ColorSpace` into a supported encoding.
fn image_color(document: &PdfDocument, dict: &Dict) -> std::result::Result<ImageColor, String> {
    let Some(value) = dict.get("ColorSpace") else {
        return Err("image has no /ColorSpace".to_string());
    };
    let color_space = match resolve_object(&document.objects, value) {
        Ok(PdfObject::Name(name)) => name.as_str(),
        Ok(PdfObject::Array(_)) => return indexed_color(document, value),
        Ok(_) => return Err("/ColorSpace must be a name or an /Indexed array".to_string()),
        Err(error) => return Err(error.message().to_string()),
    };
    match color_space {
        "DeviceGray" | "G" => Ok(ImageColor::Gray),
        "DeviceRGB" | "RGB" => Ok(ImageColor::Rgb),
        "DeviceCMYK" | "CMYK" => Ok(ImageColor::Cmyk),
        other => Err(format!("/ColorSpace /{other} is not supported for PNG output")),
    }
}

fn indexed_color(document: &PdfDocument, value: &PdfObject) -> std::result::Result<ImageColor, String> {
    let Ok(PdfObject::Array(items)) = resolve_object(&document.objects, value) else {
        return Err("/ColorSpace array is unreadable".to_string());
    };
    if items.len() < 4 || resolve_object(&document.objects, &items[0]).ok().and_then(PdfObject::as_name) != Some("Indexed") {
        return Err("/ColorSpace array is not /Indexed".to_string());
    }

    let base = match resolve_object(&document.objects, &items[1]).ok().and_then(PdfObject::as_name) {
        Some("DeviceGray") | Some("G") => 1,
        Some("DeviceRGB") | Some("RGB") => 3,
        Some(other) => return Err(format!("/Indexed base /{other} is not supported")),
        None => return Err("/Indexed has no readable base color space".to_string()),
    };
    let hival = match resolve_object(&document.objects, &items[2]) {
        Ok(PdfObject::Integer(value)) if (0..=255).contains(value) => *value as usize,
        _ => return Err("/Indexed /hival must be an integer between 0 and 255".to_string()),
    };
    let lookup = match resolve_object(&document.objects, &items[3]) {
        Ok(PdfObject::String(bytes)) => bytes.clone(),
        Ok(PdfObject::Stream(stream)) => {
            filters::decode(stream, &document.objects).map_err(|error| error.message().to_string())?
        }
        _ => return Err("/Indexed lookup table is not a string or a stream".to_string()),
    };
    if lookup.len() < (hival + 1).saturating_mul(base) {
        return Err("/Indexed lookup table is shorter than /hival requires".to_string());
    }
    Ok(ImageColor::Indexed(IndexedSpace { base, hival, lookup }))
}

/// Reads a `/Decode` array, falling back to `default` when absent.
fn decode_values(document: &PdfDocument, dict: &Dict, default: &[f64]) -> std::result::Result<Vec<f64>, String> {
    let Some(value) = dict.get("Decode") else {
        return Ok(default.to_vec());
    };
    let items = match resolve_object(&document.objects, value) {
        Ok(PdfObject::Array(items)) => items,
        Ok(_) => return Err("/Decode is not an array".to_string()),
        Err(error) => return Err(error.message().to_string()),
    };
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        match resolve_object(&document.objects, item) {
            Ok(PdfObject::Integer(value)) => values.push(*value as f64),
            Ok(PdfObject::Real(value)) => values.push(*value),
            _ => return Err("/Decode holds a non-numeric value".to_string()),
        }
    }
    Ok(values)
}

/// Maps one 8-bit sample through a decode pair into `0..=1`.
fn decode_sample(sample: u8, low: f64, high: f64) -> f64 {
    let value = low + f64::from(sample) / 255.0 * (high - low);
    value.clamp(0.0, 1.0)
}

/// Converts `/DeviceCMYK` samples to RGB, inverting first through `/Decode`.
///
/// The PDF default `/Decode [1 0 1 0 1 0 1 0]` stores CMYK inverted, so the
/// conversion happens on the decoded values.
fn cmyk_to_rgb(document: &PdfDocument, dict: &Dict, pixels: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let values = decode_values(document, dict, &[1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0])?;
    if values.len() < 8 {
        return Err("/Decode for /DeviceCMYK must hold four pairs".to_string());
    }
    let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
    for pixel in pixels.chunks_exact(4) {
        let c = decode_sample(pixel[0], values[0], values[1]);
        let m = decode_sample(pixel[1], values[2], values[3]);
        let y = decode_sample(pixel[2], values[4], values[5]);
        let k = decode_sample(pixel[3], values[6], values[7]);
        let red = (255.0 * (1.0 - c) * (1.0 - k)).round().clamp(0.0, 255.0) as u8;
        let green = (255.0 * (1.0 - m) * (1.0 - k)).round().clamp(0.0, 255.0) as u8;
        let blue = (255.0 * (1.0 - y) * (1.0 - k)).round().clamp(0.0, 255.0) as u8;
        rgb.extend_from_slice(&[red, green, blue]);
    }
    Ok(rgb)
}

/// Expands 8-bit `/Indexed` samples through the palette into RGB.
fn indexed_to_rgb(
    document: &PdfDocument,
    dict: &Dict,
    space: &IndexedSpace,
    pixels: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    let values = decode_values(document, dict, &[0.0, space.hival as f64])?;
    if values.len() < 2 {
        return Err("/Decode for /Indexed must hold a pair".to_string());
    }
    let mut rgb = Vec::with_capacity(pixels.len() * 3);
    for sample in pixels {
        let mapped = values[0] + f64::from(*sample) / 255.0 * (values[1] - values[0]);
        let index = mapped.round().clamp(0.0, space.hival as f64) as usize;
        let offset = index.saturating_mul(space.base);
        match space.base {
            1 => {
                let gray = *space
                    .lookup
                    .get(offset)
                    .ok_or_else(|| "/Indexed lookup table is shorter than /hival requires".to_string())?;
                rgb.extend_from_slice(&[gray, gray, gray]);
            }
            3 => {
                let entry = space
                    .lookup
                    .get(offset..offset + 3)
                    .ok_or_else(|| "/Indexed lookup table is shorter than /hival requires".to_string())?;
                rgb.extend_from_slice(entry);
            }
            _ => return Err("/Indexed base is not supported".to_string()),
        }
    }
    Ok(rgb)
}

fn expected_bytes(width: u32, height: u32, channels: usize) -> Option<usize> {
    let pixels = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?;
    let bytes = pixels.checked_mul(channels)?;
    if bytes as u64 > MAX_IMAGE_BYTES { None } else { Some(bytes) }
}

fn smask_alpha(document: &PdfDocument, value: &PdfObject, width: u32, height: u32) -> std::result::Result<Vec<u8>, String> {
    let stream = match resolve_object(&document.objects, value) {
        Ok(PdfObject::Stream(stream)) => stream,
        Ok(_) => return Err("/SMask is not a stream".to_string()),
        Err(error) => return Err(error.message().to_string()),
    };
    let gray = match stream.dict.get("ColorSpace") {
        Some(value) => match resolve_object(&document.objects, value) {
            Ok(PdfObject::Name(name)) => name == "DeviceGray" || name == "G",
            _ => false,
        },
        None => true,
    };
    if !gray {
        return Err("/SMask is not a DeviceGray image".to_string());
    }
    if image_number(document, &stream.dict, "Width") != Some(width)
        || image_number(document, &stream.dict, "Height") != Some(height)
    {
        return Err("/SMask dimensions do not match the image".to_string());
    }
    if image_number(document, &stream.dict, "BitsPerComponent").unwrap_or(8) != 8 {
        return Err("/SMask is not an 8-bit image".to_string());
    }
    let decoded = filters::decode(stream, &document.objects).map_err(|error| error.message().to_string())?;
    let Some(expected) = expected_bytes(width, height, 1) else {
        return Err("/SMask dimensions exceed the supported size".to_string());
    };
    if decoded.len() < expected {
        return Err("/SMask data is shorter than its dimensions require".to_string());
    }
    Ok(decoded[..expected].to_vec())
}

impl<'a> ImageCollector<'a> {
    fn push_loss(&mut self, index: usize, name: &str, stream: &PdfStream, filter: &str, message: impl Into<String>) {
        let width = image_number(self.document, &stream.dict, "Width").unwrap_or(0);
        let height = image_number(self.document, &stream.dict, "Height").unwrap_or(0);
        self.images.push(ExtractedImage {
            page: self.page,
            index,
            name: name.to_string(),
            width,
            height,
            filter: filter.to_string(),
            path: String::new(),
            loss: Some(message.into()),
        });
    }

    fn unique_path(&mut self, index: usize, extension: &str) -> PathBuf {
        let mut attempt = 1usize;
        loop {
            let suffix = if attempt == 1 { String::new() } else { format!("-{attempt}") };
            let file = format!("{}-p{}-img{}{}.{extension}", self.stem, self.page, index, suffix);
            if self.used.insert(file.clone()) {
                return self.out_dir.join(file);
            }
            attempt += 1;
        }
    }
}
