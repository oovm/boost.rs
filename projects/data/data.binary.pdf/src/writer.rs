use std::collections::BTreeMap;

use crate::error::{Result, invalid, write_error};
use crate::object::{Dict, PdfObject, PdfStream, encode_text};
use crate::parse::{PdfPage, PdfDocument, resolve_object};

/// Output object table used while assembling a new PDF.
pub struct PdfWriter {
    objects: BTreeMap<u32, PdfObject>,
    next: u32,
}

impl PdfWriter {
    pub fn new() -> Self {
        Self { objects: BTreeMap::new(), next: 1 }
    }

    /// Reserves an object number so forward references (page parents) can be
    /// written before the object body is known.
    pub fn reserve(&mut self) -> u32 {
        let number = self.next;
        self.next = self.next.saturating_add(1);
        number
    }

    pub fn set(&mut self, number: u32, object: PdfObject) {
        self.objects.insert(number, object);
    }

    pub fn add(&mut self, object: PdfObject) -> u32 {
        let number = self.reserve();
        self.set(number, object);
        number
    }

    pub fn get_mut(&mut self, number: u32) -> Option<&mut PdfObject> {
        self.objects.get_mut(&number)
    }

    pub fn get(&self, number: u32) -> Option<&PdfObject> {
        self.objects.get(&number)
    }

    /// Serializes the document as a classic PDF 1.7 file with a full xref table.
    pub fn finish(&self, root: u32, info: Option<u32>) -> Result<Vec<u8>> {
        let highest = self.objects.keys().next_back().copied().unwrap_or(0);
        for number in 1..=highest {
            if !self.objects.contains_key(&number) {
                return Err(write_error(format!("internal object {number} was reserved but never written")));
            }
        }

        let mut output = Vec::new();
        output.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

        let mut offsets = Vec::new();
        for number in 1..=highest {
            let Some(object) = self.objects.get(&number) else {
                return Err(write_error("internal object table is incomplete"));
            };
            let offset = output.len();
            offsets.push((number, offset));
            output.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            write_object(&mut output, object)?;
            output.extend_from_slice(b"\nendobj\n");
        }

        let xref_offset = output.len();
        output.extend_from_slice(b"xref\n");
        output.extend_from_slice(format!("0 {}\n", highest.saturating_add(1)).as_bytes());
        output.extend_from_slice(b"0000000000 65535 f \n");
        for (_, offset) in &offsets {
            output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }

        let mut trailer = Dict::new();
        trailer.insert("Size".to_string(), PdfObject::Integer(i64::from(highest).saturating_add(1)));
        trailer.insert("Root".to_string(), PdfObject::Reference(root, 0));
        if let Some(info) = info {
            trailer.insert("Info".to_string(), PdfObject::Reference(info, 0));
        }

        output.extend_from_slice(b"trailer\n");
        write_object(&mut output, &PdfObject::Dictionary(trailer))?;
        output.extend_from_slice(b"\nstartxref\n");
        output.extend_from_slice(format!("{xref_offset}\n").as_bytes());
        output.extend_from_slice(b"%%EOF\n");
        Ok(output)
    }
}

fn write_object(output: &mut Vec<u8>, object: &PdfObject) -> Result<()> {
    match object {
        PdfObject::Null => output.extend_from_slice(b"null"),
        PdfObject::Boolean(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        PdfObject::Integer(value) => output.extend_from_slice(value.to_string().as_bytes()),
        PdfObject::Real(value) => output.extend_from_slice(format_real(*value).as_bytes()),
        PdfObject::String(bytes) => write_literal_string(output, bytes),
        PdfObject::Name(name) => {
            output.push(b'/');
            write_name(output, name);
        }
        PdfObject::Array(items) => {
            output.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(b' ');
                }
                write_object(output, item)?;
            }
            output.push(b']');
        }
        PdfObject::Dictionary(dict) => write_dictionary(output, dict)?,
        PdfObject::Reference(number, generation) => {
            output.extend_from_slice(format!("{number} {generation} R").as_bytes());
        }
        PdfObject::Stream(stream) => {
            let mut dict = stream.dict.clone();
            dict.insert("Length".to_string(), PdfObject::Integer(stream.data.len() as i64));
            write_dictionary(output, &dict)?;
            output.extend_from_slice(b"\nstream\n");
            output.extend_from_slice(&stream.data);
            output.extend_from_slice(b"\nendstream");
        }
    }
    Ok(())
}

fn write_dictionary(output: &mut Vec<u8>, dict: &Dict) -> Result<()> {
    output.extend_from_slice(b"<<");
    for (key, value) in dict {
        output.extend_from_slice(b" /");
        write_name(output, key);
        output.push(b' ');
        write_object(output, value)?;
    }
    output.extend_from_slice(b" >>");
    Ok(())
}

fn write_name(output: &mut Vec<u8>, name: &str) {
    for byte in name.as_bytes() {
        let regular = (0x21..=0x7E).contains(byte)
            && !matches!(byte, b'#' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%');
        if regular {
            output.push(*byte);
        } else {
            output.extend_from_slice(format!("#{byte:02X}").as_bytes());
        }
    }
}

fn write_literal_string(output: &mut Vec<u8>, bytes: &[u8]) {
    output.push(b'(');
    for byte in bytes {
        match byte {
            b'(' => output.extend_from_slice(b"\\("),
            b')' => output.extend_from_slice(b"\\)"),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x08 => output.extend_from_slice(b"\\b"),
            0x0C => output.extend_from_slice(b"\\f"),
            0x20..=0x7E => output.push(*byte),
            other => output.extend_from_slice(format!("\\{:03o}", other).as_bytes()),
        }
    }
    output.push(b')');
}

fn format_real(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if text.is_empty() || text == "-0" { "0".to_string() } else { text }
}

/// Deep-copies a value from a source document into the writer, memoizing
/// indirect objects so shared resources are copied once.
pub fn copy_object(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    object: &PdfObject,
) -> Result<PdfObject> {
    match object {
        PdfObject::Reference(number, _) => copy_reference(writer, source, map, *number),
        PdfObject::Array(items) => {
            let mut copied = Vec::with_capacity(items.len());
            for item in items {
                copied.push(copy_object(writer, source, map, item)?);
            }
            Ok(PdfObject::Array(copied))
        }
        PdfObject::Dictionary(dict) => {
            let mut copied = Dict::new();
            for (key, value) in dict {
                copied.insert(key.clone(), copy_object(writer, source, map, value)?);
            }
            Ok(PdfObject::Dictionary(copied))
        }
        PdfObject::Stream(stream) => {
            let mut dict = Dict::new();
            for (key, value) in &stream.dict {
                dict.insert(key.clone(), copy_object(writer, source, map, value)?);
            }
            Ok(PdfObject::Stream(PdfStream { dict, data: stream.data.clone() }))
        }
        other => Ok(other.clone()),
    }
}

fn copy_reference(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    number: u32,
) -> Result<PdfObject> {
    if let Some(copied) = map.get(&number) {
        return Ok(PdfObject::Reference(*copied, 0));
    }
    let Some(object) = source.objects.get(&number) else {
        return Err(invalid(format!("object {number} is missing")));
    };
    let target = writer.reserve();
    map.insert(number, target);
    let copied = copy_object(writer, source, map, object)?;
    writer.set(target, copied);
    Ok(PdfObject::Reference(target, 0))
}

/// Copies one page (and every object it reaches) into the writer.
pub fn copy_page(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    page: &PdfPage,
) -> Result<u32> {
    copy_page_with_rotation(writer, source, map, page, None)
}

/// Copies one page into the writer, optionally replacing its effective
/// `/Rotate` value.
///
/// `None` keeps the copied value, `Some(0)` removes `/Rotate` entirely, and
/// any other value is written as a direct integer.
pub fn copy_page_with_rotation(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    page: &PdfPage,
    rotation: Option<i64>,
) -> Result<u32> {
    let page_number = writer.reserve();
    let mut dict = Dict::new();
    for (key, value) in &page.own {
        if key == "Parent" {
            continue;
        }
        let copied = if key == "Annots" {
            copy_annots(writer, source, map, value, page_number)?
        } else {
            copy_object(writer, source, map, value)?
        };
        dict.insert(key.clone(), copied);
    }

    for key in ["MediaBox", "CropBox", "BleedBox", "TrimBox", "ArtBox", "Resources", "Rotate", "UserUnit"] {
        if !dict.contains_key(key)
            && let Some(value) = page.inherited.get(key)
        {
            dict.insert(key.to_string(), copy_object(writer, source, map, value)?);
        }
    }
    if !dict.contains_key("MediaBox") {
        dict.insert("MediaBox".to_string(), media_box_object(page.media_box));
    }
    if !dict.contains_key("Resources") {
        dict.insert("Resources".to_string(), PdfObject::Dictionary(Dict::new()));
    }
    if let Some(rotation) = rotation {
        dict.remove("Rotate");
        if rotation != 0 {
            dict.insert("Rotate".to_string(), PdfObject::Integer(rotation));
        }
    }
    dict.insert("Type".to_string(), PdfObject::Name("Page".to_string()));

    writer.set(page_number, PdfObject::Dictionary(dict));
    Ok(page_number)
}

fn copy_annots(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    value: &PdfObject,
    page: u32,
) -> Result<PdfObject> {
    let resolved = resolve_object(&source.objects, value)?;
    if let PdfObject::Array(items) = resolved {
        let mut copied = Vec::with_capacity(items.len());
        for item in items {
            copied.push(copy_annotation(writer, source, map, item, page)?);
        }
        Ok(PdfObject::Array(copied))
    } else {
        copy_object(writer, source, map, value)
    }
}

fn copy_annotation(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    annotation: &PdfObject,
    page: u32,
) -> Result<PdfObject> {
    let has_page = match resolve_object(&source.objects, annotation) {
        Ok(PdfObject::Dictionary(dict)) => dict.contains_key("P"),
        _ => false,
    };
    let copied = copy_object(writer, source, map, annotation)?;
    if !has_page {
        return Ok(copied);
    }
    match copied {
        PdfObject::Reference(number, _) => {
            if let Some(PdfObject::Dictionary(dict)) = writer.get_mut(number) {
                dict.insert("P".to_string(), PdfObject::Reference(page, 0));
            }
            Ok(PdfObject::Reference(number, 0))
        }
        PdfObject::Dictionary(mut dict) => {
            dict.insert("P".to_string(), PdfObject::Reference(page, 0));
            Ok(PdfObject::Dictionary(dict))
        }
        other => Ok(other),
    }
}

/// Copies the document information dictionary, optionally replacing the title.
pub fn copy_info(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    title: Option<&str>,
) -> Result<Option<u32>> {
    let edits: Vec<(&str, Option<&str>)> = title.map(|title| vec![("Title", Some(title))]).unwrap_or_default();
    copy_info_fields(writer, source, map, &edits)
}

/// Copies the document information dictionary, applying keyed edits.
///
/// `Some(text)` replaces a field, `None` removes it. Fields not present in
/// `edits` keep their source value.
pub fn copy_info_fields(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    edits: &[(&str, Option<&str>)],
) -> Result<Option<u32>> {
    let mut dict = match source.info {
        Some(number) => match source.objects.get(&number) {
            Some(object) => match copy_object(writer, source, map, object)? {
                PdfObject::Dictionary(dict) => dict,
                _ => Dict::new(),
            },
            None => return Err(invalid(format!("/Info object {number} is missing"))),
        },
        None => Dict::new(),
    };
    for (key, value) in edits {
        match value {
            Some(text) => {
                dict.insert((*key).to_string(), PdfObject::String(encode_text(text)));
            }
            None => {
                dict.remove(*key);
            }
        }
    }
    if dict.is_empty() {
        return Ok(None);
    }
    Ok(Some(writer.add(PdfObject::Dictionary(dict))))
}

pub fn media_box_object(media_box: [f64; 4]) -> PdfObject {
    PdfObject::Array(media_box.iter().map(|value| PdfObject::Real(*value)).collect())
}
