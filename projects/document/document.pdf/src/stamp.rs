use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use crate::create::{FontFace, number, push_literal, text_width};
use crate::error::{Result, argument, invalid};
use crate::object::{Dict, PdfObject, PdfStream};
use crate::parse::PdfPage;
use crate::writer::{PdfWriter, copy_info, copy_page};
use crate::{StampReport, StampRequest, finish_document, load_document, parse_ranges, write_file};

/// Where a stamp is painted on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Center,
    Top,
    Bottom,
}

impl Position {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()).unwrap_or("center").to_ascii_lowercase().as_str() {
            "center" | "middle" | "diagonal" => Ok(Self::Center),
            "top" | "header" => Ok(Self::Top),
            "bottom" | "footer" => Ok(Self::Bottom),
            other => Err(argument(format!("stamp position `{other}` is not `center`, `top` or `bottom`"))),
        }
    }
}

struct Settings {
    position: Position,
    font: FontFace,
    font_size: f64,
    rotation: f64,
    color: (f64, f64, f64),
}

/// Paints a text stamp on the selected pages and writes a new PDF.
pub(crate) fn stamp(input: &Path, out: &Path, request: &StampRequest) -> Result<StampReport> {
    if request.text.trim().is_empty() {
        return Err(argument("stamp needs a non-empty text"));
    }
    let document = load_document(input)?;
    let total = document.pages.len();
    let ranges = match request.pages.as_deref().filter(|value| !value.trim().is_empty()) {
        Some(value) => parse_ranges(value, total)?,
        None => vec![(1, total)],
    };
    let mut selected = BTreeSet::new();
    for (start, end) in ranges {
        for page in start..=end {
            selected.insert(page);
        }
    }

    let position = Position::parse(request.position.as_deref())?;
    let font = FontFace::parse(request.font.as_deref())?;
    let font_size = request.font_size.unwrap_or(if position == Position::Center { 36.0 } else { 12.0 });
    if !font_size.is_finite() || !(4.0..=144.0).contains(&font_size) {
        return Err(argument("stamp font size must be between 4 and 144 points"));
    }
    let rotation = request.rotation.unwrap_or(0.0);
    if !rotation.is_finite() || !(-180.0..=180.0).contains(&rotation) {
        return Err(argument("stamp rotation must be between -180 and 180 degrees"));
    }
    let opacity = request.opacity.unwrap_or(if position == Position::Center { 0.3 } else { 0.6 });
    if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
        return Err(argument("stamp opacity must be between 0 and 1"));
    }
    let settings =
        Settings { position, font, font_size, rotation, color: parse_color(request.color.as_deref())? };

    let mut writer = PdfWriter::new();
    let font_number = writer.add(PdfObject::Dictionary(font_dictionary(font)));
    let mut graphics = Dict::new();
    graphics.insert("Type".to_string(), PdfObject::Name("ExtGState".to_string()));
    graphics.insert("ca".to_string(), PdfObject::Real(opacity));
    graphics.insert("CA".to_string(), PdfObject::Real(opacity));
    let graphics_number = writer.add(PdfObject::Dictionary(graphics));

    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(total);
    for (index, page) in document.pages.iter().enumerate() {
        let number = index + 1;
        let page_number = copy_page(&mut writer, &document, &mut map, page)?;
        if selected.contains(&number) {
            let text = request.text.replace("{page}", &number.to_string()).replace("{pages}", &total.to_string());
            let stream = writer.add(PdfObject::Stream(PdfStream {
                dict: Dict::new(),
                data: stamp_content(&text, page, &settings),
            }));
            attach_content(&mut writer, page_number, stream)?;
            attach_resources(&mut writer, page_number, font_number, graphics_number)?;
        }
        pages.push(page_number);
    }

    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, true)?;
    Ok(StampReport {
        path: out.display().to_string(),
        pages: total,
        pages_stamped: selected.len(),
        bytes: bytes.len(),
    })
}

/// Parses `#RGB`, `#RRGGBB` or returns mid-grey.
fn parse_color(value: Option<&str>) -> Result<(f64, f64, f64)> {
    let Some(raw) = value.map(str::trim).filter(|value| !value.is_empty())
    else {
        return Ok((0.45, 0.45, 0.45));
    };
    let hex = raw.trim_start_matches('#');
    let expanded = match hex.len() {
        3 if hex.chars().all(|character| character.is_ascii_hexdigit()) => {
            hex.chars().flat_map(|character| [character, character]).collect::<String>()
        }
        6 if hex.chars().all(|character| character.is_ascii_hexdigit()) => hex.to_string(),
        _ => return Err(argument("stamp color must be a 3- or 6-digit hex colour such as #808080")),
    };
    let channel = |start: usize| {
        u8::from_str_radix(&expanded[start..start + 2], 16).map(|value| f64::from(value) / 255.0).unwrap_or(0.0)
    };
    Ok((channel(0), channel(2), channel(4)))
}

fn font_dictionary(font: FontFace) -> Dict {
    let mut dictionary = Dict::new();
    dictionary.insert("Type".to_string(), PdfObject::Name("Font".to_string()));
    dictionary.insert("Subtype".to_string(), PdfObject::Name("Type1".to_string()));
    dictionary.insert("BaseFont".to_string(), PdfObject::Name(font.base_font().to_string()));
    dictionary.insert("Encoding".to_string(), PdfObject::Name("WinAnsiEncoding".to_string()));
    dictionary
}

fn stamp_content(text: &str, page: &PdfPage, settings: &Settings) -> Vec<u8> {
    let x0 = page.media_box[0];
    let y0 = page.media_box[1];
    let width = page.media_box[2] - x0;
    let height = page.media_box[3] - y0;
    let text_width = text_width(text, settings.font, settings.font_size);
    let (red, green, blue) = settings.color;

    let mut stream = String::from("q\n/OPStampGS gs\n");
    stream.push_str(&format!(
        "{} {} {} rg\n",
        number(red),
        number(green),
        number(blue)
    ));
    match settings.position {
        Position::Center => {
            let radians = settings.rotation.to_radians();
            let cos = radians.cos();
            let sin = radians.sin();
            let cx = x0 + width / 2.0;
            let cy = y0 + height / 2.0;
            stream.push_str(&format!(
                "{} {} {} {} {} {} cm\nBT\n/OPStampF {} Tf\n1 0 0 1 {} {} Tm\n",
                number(cos),
                number(sin),
                number(-sin),
                number(cos),
                number(cx),
                number(cy),
                number(settings.font_size),
                number(-text_width / 2.0),
                number(-settings.font_size * 0.35)
            ));
        }
        Position::Top | Position::Bottom => {
            let x = x0 + ((width - text_width) / 2.0).max(0.0);
            let y = if settings.position == Position::Top { page.media_box[3] - 48.0 } else { y0 + 36.0 };
            stream.push_str(&format!(
                "BT\n/OPStampF {} Tf\n1 0 0 1 {} {} Tm\n",
                number(settings.font_size),
                number(x),
                number(y)
            ));
        }
    }
    stream.push('(');
    push_literal(&mut stream, text);
    stream.push_str(") Tj\nET\nQ\n");
    stream.into_bytes()
}

/// Appends a content stream to `/Contents`, keeping the original content.
fn attach_content(writer: &mut PdfWriter, page: u32, content: u32) -> Result<()> {
    let existing = writer.get(page).and_then(PdfObject::as_dict).and_then(|dict| dict.get("Contents")).cloned();
    let combined = match existing {
        None | Some(PdfObject::Null) => PdfObject::Reference(content, 0),
        Some(PdfObject::Array(mut items)) => {
            items.push(PdfObject::Reference(content, 0));
            PdfObject::Array(items)
        }
        Some(PdfObject::Reference(number, _)) => {
            // `/Contents` may point at either one stream or an array of
            // streams; keep an existing array flat instead of nesting it.
            match writer.get(number) {
                Some(PdfObject::Array(items)) => {
                    let mut items = items.clone();
                    items.push(PdfObject::Reference(content, 0));
                    PdfObject::Array(items)
                }
                _ => PdfObject::Array(vec![PdfObject::Reference(number, 0), PdfObject::Reference(content, 0)]),
            }
        }
        Some(PdfObject::Stream(_)) => {
            return Err(invalid("a direct /Contents stream cannot be combined with a stamp"));
        }
        Some(_) => return Err(invalid("/Contents is neither a stream nor an array of streams")),
    };
    match writer.get_mut(page) {
        Some(PdfObject::Dictionary(dict)) => {
            dict.insert("Contents".to_string(), combined);
            Ok(())
        }
        _ => Err(invalid("the copied page is not a dictionary")),
    }
}

/// Adds the stamp font and graphics state to the page resources.
fn attach_resources(writer: &mut PdfWriter, page: u32, font: u32, graphics: u32) -> Result<()> {
    let existing = writer.get(page).and_then(PdfObject::as_dict).and_then(|dict| dict.get("Resources")).cloned();
    let mut resources = match existing {
        Some(PdfObject::Dictionary(dict)) => dict,
        Some(PdfObject::Reference(number, _)) => {
            writer.get(number).and_then(PdfObject::as_dict).cloned().unwrap_or_default()
        }
        _ => Dict::new(),
    };
    insert_resource(writer, &mut resources, "Font", "OPStampF", PdfObject::Reference(font, 0));
    insert_resource(writer, &mut resources, "ExtGState", "OPStampGS", PdfObject::Reference(graphics, 0));
    match writer.get_mut(page) {
        Some(PdfObject::Dictionary(dict)) => {
            dict.insert("Resources".to_string(), PdfObject::Dictionary(resources));
            Ok(())
        }
        _ => Err(invalid("the copied page is not a dictionary")),
    }
}

fn insert_resource(writer: &PdfWriter, resources: &mut Dict, group: &str, name: &str, value: PdfObject) {
    let mut entries = match resources.get(group) {
        Some(PdfObject::Dictionary(dict)) => dict.clone(),
        Some(PdfObject::Reference(number, _)) => {
            writer.get(*number).and_then(PdfObject::as_dict).cloned().unwrap_or_default()
        }
        _ => Dict::new(),
    };
    entries.insert(name.to_string(), value);
    resources.insert(group.to_string(), PdfObject::Dictionary(entries));
}
