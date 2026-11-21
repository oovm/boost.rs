use crate::CreateRequest;
use crate::error::{Result, argument};
use crate::object::{Dict, PdfObject, PdfStream, encode_text};
use crate::writer::PdfWriter;

const DEFAULT_WIDTH: f64 = 612.0;
const DEFAULT_HEIGHT: f64 = 792.0;
const DEFAULT_MARGIN: f64 = 72.0;
const DEFAULT_FONT_SIZE: f64 = 12.0;
const MIN_FONT_SIZE: f64 = 6.0;
const MAX_FONT_SIZE: f64 = 144.0;
const MIN_MARGIN: f64 = 18.0;

/// Horizontal alignment of body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()).unwrap_or("left").to_ascii_lowercase().as_str() {
            "left" => Ok(Self::Left),
            "center" | "centre" => Ok(Self::Center),
            "right" => Ok(Self::Right),
            other => Err(argument(format!("alignment `{other}` is not `left`, `center` or `right`"))),
        }
    }
}

/// One of the PDF base-14 fonts this engine can address by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontFace {
    Helvetica,
    HelveticaBold,
    Times,
    TimesBold,
    Courier,
    CourierBold,
}

impl FontFace {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        let name = value.map(str::trim).filter(|value| !value.is_empty()).unwrap_or("helvetica").to_ascii_lowercase();
        match name.as_str() {
            "helvetica" | "sans" | "sans-serif" => Ok(Self::Helvetica),
            "helvetica-bold" | "sans-bold" | "bold" => Ok(Self::HelveticaBold),
            "times" | "serif" | "times-roman" => Ok(Self::Times),
            "times-bold" | "serif-bold" => Ok(Self::TimesBold),
            "courier" | "mono" | "monospace" => Ok(Self::Courier),
            "courier-bold" | "mono-bold" => Ok(Self::CourierBold),
            other => Err(argument(format!(
                "font `{other}` is not helvetica, helvetica-bold, times, times-bold, courier or courier-bold"
            ))),
        }
    }

    pub(crate) fn base_font(self) -> &'static str {
        match self {
            Self::Helvetica => "Helvetica",
            Self::HelveticaBold => "Helvetica-Bold",
            Self::Times => "Times-Roman",
            Self::TimesBold => "Times-Bold",
            Self::Courier => "Courier",
            Self::CourierBold => "Courier-Bold",
        }
    }

    fn wide(self) -> bool {
        matches!(self, Self::Courier | Self::CourierBold)
    }
}

/// Bytes plus the number of pages the composer actually emitted.
pub(crate) struct BuildOutcome {
    pub bytes: Vec<u8>,
    pub pages: usize,
}

struct Style {
    width: f64,
    height: f64,
    margin: f64,
    font: FontFace,
    font_size: f64,
    leading: f64,
    align: Align,
    page_numbers: bool,
}

/// Builds a fresh PDF with one text page per input string, paginating long
/// pages and applying the requested geometry, font and alignment.
pub(crate) fn build(request: &CreateRequest) -> Result<BuildOutcome> {
    let (width, height) = page_size(request.page_size.as_deref())?;
    let font = FontFace::parse(request.font.as_deref())?;
    let font_size = request.font_size.unwrap_or(DEFAULT_FONT_SIZE);
    if !font_size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&font_size) {
        return Err(argument(format!("font size must be between {MIN_FONT_SIZE} and {MAX_FONT_SIZE} points")));
    }
    let margin = request.margin_pt.unwrap_or(DEFAULT_MARGIN);
    let max_margin = (width.min(height) / 2.0) - MIN_MARGIN;
    if !margin.is_finite() || !(MIN_MARGIN..=max_margin).contains(&margin) {
        return Err(argument(format!("margin must be between {MIN_MARGIN} and {max_margin} points for this page size")));
    }
    let align = Align::parse(request.align.as_deref())?;
    let style = Style {
        width,
        height,
        margin,
        font,
        font_size,
        leading: (font_size * 1.3).max(font_size + 2.0),
        align,
        page_numbers: request.page_numbers.unwrap_or(false),
    };

    let usable_width = (width - margin * 2.0).max(font_size);
    let footer = if style.page_numbers { font_size + 6.0 } else { 0.0 };
    let per_page = (((height - margin * 2.0 - footer) / style.leading).floor().max(1.0)) as usize;

    let mut pages: Vec<Vec<String>> = Vec::new();
    for text in &request.pages {
        let lines = wrap_text(text, usable_width, font, font_size);
        if lines.is_empty() {
            pages.push(Vec::new());
            continue;
        }
        for chunk in lines.chunks(per_page) {
            pages.push(chunk.to_vec());
        }
    }
    let total = pages.len();

    let mut writer = PdfWriter::new();
    let font_number = writer.add(PdfObject::Dictionary(font_dictionary(font)));
    let pages_number = writer.reserve();
    let mut kids = Vec::with_capacity(pages.len());

    for (index, lines) in pages.iter().enumerate() {
        let content = writer.add(PdfObject::Stream(PdfStream {
            dict: Dict::new(),
            data: page_content(lines, &style, index + 1, total),
        }));
        let mut page = Dict::new();
        page.insert("Type".to_string(), PdfObject::Name("Page".to_string()));
        page.insert("Parent".to_string(), PdfObject::Reference(pages_number, 0));
        page.insert(
            "MediaBox".to_string(),
            PdfObject::Array(vec![
                PdfObject::Integer(0),
                PdfObject::Integer(0),
                PdfObject::Real(width),
                PdfObject::Real(height),
            ]),
        );
        page.insert("Contents".to_string(), PdfObject::Reference(content, 0));
        page.insert("Resources".to_string(), PdfObject::Dictionary(resources(font_number)));
        kids.push(PdfObject::Reference(writer.add(PdfObject::Dictionary(page)), 0));
    }

    let mut pages_tree = Dict::new();
    pages_tree.insert("Type".to_string(), PdfObject::Name("Pages".to_string()));
    pages_tree.insert("Kids".to_string(), PdfObject::Array(kids));
    pages_tree.insert("Count".to_string(), PdfObject::Integer(pages.len() as i64));
    writer.set(pages_number, PdfObject::Dictionary(pages_tree));

    let mut catalog = Dict::new();
    catalog.insert("Type".to_string(), PdfObject::Name("Catalog".to_string()));
    catalog.insert("Pages".to_string(), PdfObject::Reference(pages_number, 0));
    let root = writer.add(PdfObject::Dictionary(catalog));

    let mut info = Dict::new();
    for (key, value) in [
        ("Title", request.title.as_deref()),
        ("Author", request.author.as_deref()),
        ("Subject", request.subject.as_deref()),
        ("Keywords", request.keywords.as_deref()),
        ("Creator", request.creator.as_deref()),
    ] {
        if let Some(text) = value
            && !text.is_empty()
        {
            info.insert(key.to_string(), PdfObject::String(encode_text(text)));
        }
    }
    info.insert("Producer".to_string(), PdfObject::String(b"OpenPDF".to_vec()));
    let info_number = writer.add(PdfObject::Dictionary(info));

    let bytes = writer.finish(root, Some(info_number))?;
    Ok(BuildOutcome { bytes, pages: pages.len() })
}

/// Resolves a named paper size, an explicit `WIDTHxHEIGHT` extent or the
/// default Letter geometry.
pub(crate) fn page_size(value: Option<&str>) -> Result<(f64, f64)> {
    let Some(raw) = value.map(str::trim).filter(|value| !value.is_empty())
    else {
        return Ok((DEFAULT_WIDTH, DEFAULT_HEIGHT));
    };
    let lower = raw.to_ascii_lowercase();
    let (width, height) = match lower.as_str() {
        "a3" => (841.89, 1190.55),
        "a4" => (595.28, 841.89),
        "a5" => (419.53, 595.28),
        "letter" | "us-letter" => (612.0, 792.0),
        "legal" | "us-legal" => (612.0, 1008.0),
        "tabloid" => (792.0, 1224.0),
        other => {
            let Some((width, height)) = other.split_once('x') else {
                return Err(argument(format!(
                    "page size `{raw}` is not a named size or a WIDTHxHEIGHT extent such as 400x600"
                )));
            };
            let width: f64 = width.trim().parse().map_err(|_| argument(format!("page width in `{raw}` is not a number")))?;
            let height: f64 = height.trim().parse().map_err(|_| argument(format!("page height in `{raw}` is not a number")))?;
            (width, height)
        }
    };
    if !width.is_finite() || !height.is_finite() || !(100.0..=14400.0).contains(&width) || !(100.0..=14400.0).contains(&height)
    {
        return Err(argument("page width and height must be between 100 and 14400 points"));
    }
    Ok((width, height))
}

fn font_dictionary(font: FontFace) -> Dict {
    let mut dictionary = Dict::new();
    dictionary.insert("Type".to_string(), PdfObject::Name("Font".to_string()));
    dictionary.insert("Subtype".to_string(), PdfObject::Name("Type1".to_string()));
    dictionary.insert("BaseFont".to_string(), PdfObject::Name(font.base_font().to_string()));
    dictionary.insert("Encoding".to_string(), PdfObject::Name("WinAnsiEncoding".to_string()));
    dictionary
}

fn resources(font: u32) -> Dict {
    let mut fonts = Dict::new();
    fonts.insert("F1".to_string(), PdfObject::Reference(font, 0));
    let mut resources = Dict::new();
    resources.insert("Font".to_string(), PdfObject::Dictionary(fonts));
    resources.insert(
        "ProcSet".to_string(),
        PdfObject::Array(vec![PdfObject::Name("PDF".to_string()), PdfObject::Name("Text".to_string())]),
    );
    resources
}

fn page_content(lines: &[String], style: &Style, page: usize, total: usize) -> Vec<u8> {
    let mut stream = String::from("BT\n");
    stream.push_str(&format!("/F1 {} Tf\n", number(style.font_size)));
    let mut y = style.height - style.margin;
    let usable = style.width - style.margin * 2.0;
    for line in lines {
        let width = text_width(line, style.font, style.font_size);
        let x = match style.align {
            Align::Left => style.margin,
            Align::Center => style.margin + ((usable - width) / 2.0).max(0.0),
            Align::Right => style.width - style.margin - width,
        };
        stream.push_str(&format!("1 0 0 1 {} {} Tm\n", number(x), number(y)));
        if !line.is_empty() {
            stream.push('(');
            push_literal(&mut stream, line);
            stream.push_str(") Tj\n");
        }
        y -= style.leading;
    }
    stream.push_str("ET\n");

    if style.page_numbers {
        let label = format!("{page} / {total}");
        let size = 9.0;
        let width = text_width(&label, style.font, size);
        let x = ((style.width - width) / 2.0).max(0.0);
        let y = (style.margin / 2.0).max(12.0);
        stream.push_str("q\nBT\n");
        stream.push_str(&format!("/F1 {size} Tf\n0.35 g\n1 0 0 1 {} {} Tm\n", number(x), number(y)));
        stream.push('(');
        push_literal(&mut stream, &label);
        stream.push_str(") Tj\nET\nQ\n");
    }
    stream.into_bytes()
}

/// Splits text into wrapped lines that fit `max_width`.
fn wrap_text(text: &str, max_width: f64, font: FontFace, font_size: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw.split(' ') {
            let candidate = if current.is_empty() { word.to_string() } else { format!("{current} {word}") };
            if !current.is_empty() && text_width(&candidate, font, font_size) > max_width {
                lines.push(current);
                current = word.to_string();
            }
            else {
                current = candidate;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

/// Approximate advance width of one line in points.
///
/// The composer only ships the base-14 fonts, so a compact class table keeps
/// wrapping and alignment close enough without embedding an AFM table.
pub(crate) fn text_width(text: &str, font: FontFace, font_size: f64) -> f64 {
    let units: f64 = text.chars().map(|character| char_units(character, font)).sum();
    units * font_size
}

fn char_units(character: char, font: FontFace) -> f64 {
    if font.wide() {
        return 0.6;
    }
    match character {
        ' ' => 0.28,
        'i' | 'j' | 'l' | 'I' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' => 0.28,
        'f' | 't' | 'r' | 'J' | '(' | ')' | '[' | ']' | '/' | '\\' => 0.36,
        'm' | 'w' | 'M' | 'W' | '@' | '%' => 0.86,
        '0'..='9' => 0.556,
        'A'..='Z' => 0.68,
        'a'..='z' => 0.52,
        _ => 0.5,
    }
}

pub(crate) fn number(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let mut text = format!("{value:.3}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if text.is_empty() || text == "-0" { "0".to_string() } else { text }
}

pub(crate) fn push_literal(output: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '(' => output.push_str("\\("),
            ')' => output.push_str("\\)"),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            other => {
                let code = other as u32;
                if (0x20..=0x7E).contains(&code) {
                    output.push(other);
                }
                else {
                    let fallback = if code <= 0xFF { code } else { u32::from(b'?') };
                    output.push_str(&format!("\\{fallback:03o}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_escapes_parentheses_and_backslashes() {
        let mut output = String::new();
        push_literal(&mut output, "a(b)c\\d");
        assert_eq!(output, "a\\(b\\)c\\\\d");
    }

    #[test]
    fn content_stream_moves_between_lines() {
        let content = String::from_utf8(page_content(
            &["first".to_string(), "second".to_string()],
            &Style {
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
                margin: DEFAULT_MARGIN,
                font: FontFace::Helvetica,
                font_size: DEFAULT_FONT_SIZE,
                leading: 15.0,
                align: Align::Left,
                page_numbers: false,
            },
            1,
            1,
        ))
        .expect("utf-8");
        assert!(content.contains("(first) Tj"));
        assert!(content.contains("(second) Tj"));
        assert!(content.contains("Tm"));
    }

    #[test]
    fn page_sizes_and_wrapping_are_validated() {
        assert_eq!(page_size(None).unwrap(), (612.0, 792.0));
        assert_eq!(page_size(Some("letter")).unwrap(), (612.0, 792.0));
        assert_eq!(page_size(Some("400x600")).unwrap(), (400.0, 600.0));
        assert!(page_size(Some("wide")).is_err());
        assert!(Align::parse(Some("middle")).is_err());
        assert_eq!(Align::parse(Some("center")).unwrap(), Align::Center);
        let lines = wrap_text("one two three four", 40.0, FontFace::Helvetica, 12.0);
        assert!(lines.len() > 1, "narrow columns wrap: {lines:?}");
    }
}
