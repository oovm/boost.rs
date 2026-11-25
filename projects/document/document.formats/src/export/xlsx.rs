//! SpreadsheetML writer.
//!
//! The writer consumes the same format-native [`XlsxWorkbook`] the reader
//! produces, so a read → edit → write round trip keeps sheets, values,
//! formulas, styles, merges, column widths, row heights and frozen panes
//! instead of lowering through the semantic IR and back.
//!
//! Charts, pictures, comments, conditional formatting, data validation and
//! macro payloads have no writer here. They are reported as loss markers
//! rather than silently dropped. Defined names are written back verbatim.

use std::collections::{BTreeMap, HashMap};

use data_binary_zip::{ZipEntry, ZipWriter};
use document_ir::{LossMarker, SemanticStatus};

use super::super::import::xlsx::model::{
    CellStyle, CellValue, DateSystem, HorizontalAlign, SheetVisibility, VerticalAlign, WorkbookStyles, XlsxCell, XlsxSheet, XlsxWorkbook,
};
use crate::import::xlsx::styles::{builtin_number_format, resolve_color};
use crate::FormatError;

/// Loss code for a skipped chart or drawing frame.
pub const LOSS_CHART: &str = "export.xlsx.chart";
/// Loss code for a skipped anchored picture.
pub const LOSS_IMAGE: &str = "export.xlsx.image";
/// Loss code for a skipped cell comment.
pub const LOSS_COMMENT: &str = "export.xlsx.comment";
/// Loss code for skipped conditional formatting rules.
pub const LOSS_CONDITIONAL_FORMATTING: &str = "export.xlsx.conditional-formatting";
/// Loss code for skipped data validation rules.
pub const LOSS_DATA_VALIDATION: &str = "export.xlsx.data-validation";
/// Loss code for a workbook macro payload that cannot be written as `.xlsx`.
pub const LOSS_MACRO: &str = "export.xlsx.macro";
/// Loss code for a skipped embedded object.
pub const LOSS_EMBEDDED_OBJECT: &str = "export.xlsx.embedded-object";

/// Written package plus the constructs the writer had to skip.
pub struct XlsxExport {
    /// The `.xlsx` package bytes.
    pub bytes: Vec<u8>,
    /// Declared losses, one marker per skipped construct kind.
    pub losses: Vec<LossMarker>,
}

/// Writes a workbook as an `.xlsx` package.
pub fn export_xlsx_bytes(workbook: &XlsxWorkbook) -> Result<Vec<u8>, FormatError> {
    Ok(export_xlsx_bytes_report(workbook)?.bytes)
}

/// Writes a workbook as an `.xlsx` package and reports declared losses.
pub fn export_xlsx_bytes_report(workbook: &XlsxWorkbook) -> Result<XlsxExport, FormatError> {
    if workbook.sheets.is_empty() {
        return Err(FormatError::invalid_input("an XLSX workbook needs at least one sheet"));
    }
    let table = StyleTable::build(workbook);
    let mut writer = ZipWriter::new();
    writer.push(ZipEntry::deflated("[Content_Types].xml", content_types(workbook)));
    writer.push(ZipEntry::deflated("_rels/.rels", root_rels()));
    writer.push(ZipEntry::deflated("xl/workbook.xml", workbook_xml(workbook)));
    writer.push(ZipEntry::deflated("xl/_rels/workbook.xml.rels", workbook_rels(workbook)));
    for (index, sheet) in workbook.sheets.iter().enumerate() {
        writer.push(ZipEntry::deflated(format!("xl/worksheets/sheet{}.xml", index + 1), sheet_xml(sheet, &table)));
    }
    writer.push(ZipEntry::deflated("xl/styles.xml", table.xml()));
    let bytes = writer.into_bytes().map_err(|error| FormatError::parse("xlsx", format!("failed to assemble the package: {error}")))?;
    Ok(XlsxExport { bytes, losses: losses(workbook) })
}

fn losses(workbook: &XlsxWorkbook) -> Vec<LossMarker> {
    let mut chart = false;
    let mut image = false;
    let mut comment = false;
    let mut conditional = false;
    let mut validation = false;
    let mut embedded = false;
    for sheet in &workbook.sheets {
        chart |= sheet.charts > 0;
        image |= !sheet.images.is_empty();
        comment |= !sheet.comments.is_empty();
        conditional |= sheet.conditional_formatting;
        validation |= sheet.data_validations;
        embedded |= sheet.embedded_objects > 0;
    }
    let markers = [
        (chart, LOSS_CHART, "charts and drawing frames are not written"),
        (image, LOSS_IMAGE, "anchored pictures are not written"),
        (comment, LOSS_COMMENT, "cell comments are not written"),
        (conditional, LOSS_CONDITIONAL_FORMATTING, "conditional formatting rules are not written"),
        (validation, LOSS_DATA_VALIDATION, "data validation rules are not written"),
        (workbook.macro_enabled, LOSS_MACRO, "a macro payload cannot be written as .xlsx"),
        (embedded, LOSS_EMBEDDED_OBJECT, "embedded objects are not written"),
    ];
    markers
        .into_iter()
        .filter(|(seen, _, _)| *seen)
        .map(|(_, code, message)| LossMarker { code: code.to_string(), message: message.to_string(), status: SemanticStatus::Partial })
        .collect()
}

fn content_types(workbook: &XlsxWorkbook) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>");
    body.push_str("<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">");
    body.push_str("<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>");
    body.push_str("<Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    body.push_str("<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>");
    body.push_str("<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>");
    for index in 0..workbook.sheets.len() {
        body.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            index + 1
        ));
    }
    body.push_str("</Types>");
    body
}

fn root_rels() -> String {
    concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>",
        "</Relationships>"
    )
    .to_owned()
}

fn workbook_xml(workbook: &XlsxWorkbook) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>");
    body.push_str("<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">");
    if workbook.date_system == DateSystem::Date1904 {
        body.push_str("<workbookPr date1904=\"1\"/>");
    }
    body.push_str("<bookViews><workbookView activeTab=\"0\"/></bookViews>");
    body.push_str("<sheets>");
    for (index, sheet) in workbook.sheets.iter().enumerate() {
        let state = match sheet.visibility {
            SheetVisibility::Visible => String::new(),
            SheetVisibility::Hidden => " state=\"hidden\"".to_owned(),
            SheetVisibility::VeryHidden => " state=\"veryHidden\"".to_owned(),
        };
        body.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{}\"{} r:id=\"rId{}\"/>",
            escape_attribute(&sheet.name),
            index + 1,
            state,
            index + 1
        ));
    }
    body.push_str("</sheets>");
    if !workbook.defined_names.is_empty() {
        body.push_str("<definedNames>");
        for defined in &workbook.defined_names {
            let local = defined
                .sheet
                .as_deref()
                .and_then(|name| workbook.sheets.iter().position(|sheet| sheet.name == *name))
                .map(|index| format!(" localSheetId=\"{index}\""))
                .unwrap_or_default();
            body.push_str(&format!(
                "<definedName name=\"{}\"{local}>{}</definedName>",
                escape_attribute(&defined.name),
                escape_text(defined.reference.trim())
            ));
        }
        body.push_str("</definedNames>");
    }
    body.push_str("</workbook>");
    body
}

fn workbook_rels(workbook: &XlsxWorkbook) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>");
    body.push_str("<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">");
    for index in 0..workbook.sheets.len() {
        body.push_str(&format!(
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
            index + 1,
            index + 1
        ));
    }
    body.push_str(&format!(
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
        workbook.sheets.len() + 1
    ));
    body.push_str("</Relationships>");
    body
}

fn sheet_xml(sheet: &XlsxSheet, table: &StyleTable) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>");
    body.push_str("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">");
    let last_row = sheet.last_row();
    let last_column = sheet.last_column();
    let dimension = match sheet.dimension {
        Some(declared) if declared.end_row >= last_row && declared.end_column >= last_column => declared,
        Some(_) | None if last_row > 0 && last_column > 0 => crate::import::xlsx::model::CellRange {
            start_row: 1,
            start_column: 1,
            end_row: last_row,
            end_column: last_column,
        },
        Some(declared) => declared,
        None => crate::import::xlsx::model::CellRange { start_row: 1, start_column: 1, end_row: 1, end_column: 1 },
    };
    body.push_str(&format!(
        "<dimension ref=\"A1:{}\"/>",
        crate::a1::format_reference(dimension.end_row, dimension.end_column)
    ));
    body.push_str("<sheetViews><sheetView workbookViewId=\"0\">");
    if let Some(freeze) = sheet.freeze.filter(|freeze| freeze.columns > 0 || freeze.rows > 0) {
        let top_left = crate::a1::format_reference(freeze.rows + 1, freeze.columns + 1);
        body.push_str(&format!(
            "<pane xSplit=\"{}\" ySplit=\"{}\" topLeftCell=\"{top_left}\" activePane=\"bottomRight\" state=\"frozen\"/>",
            freeze.columns, freeze.rows
        ));
    }
    body.push_str("</sheetView></sheetViews>");
    let default_column = if (sheet.default_column_width - 8.43).abs() > f64::EPSILON {
        format!(" defaultColWidth=\"{}\"", format_number(sheet.default_column_width))
    }
    else {
        String::new()
    };
    body.push_str(&format!("<sheetFormatPr defaultRowHeight=\"{}\"{default_column}/>", format_number(sheet.default_row_height)));
    if !sheet.columns.is_empty() {
        body.push_str("<cols>");
        for column in &sheet.columns {
            body.push_str(&format!(
                "<col min=\"{}\" max=\"{}\" width=\"{}\"{}{}/>",
                column.min,
                column.max,
                format_number(column.width),
                if column.custom { " customWidth=\"1\"" } else { "" },
                if column.hidden { " hidden=\"1\"" } else { "" }
            ));
        }
        body.push_str("</cols>");
    }

    body.push_str("<sheetData>");
    let mut rows: BTreeMap<u32, Vec<&XlsxCell>> = BTreeMap::new();
    for cell in &sheet.cells {
        rows.entry(cell.row).or_default().push(cell);
    }
    for (row, cells) in &rows {
        body.push_str(&format!("<row r=\"{row}\""));
        if let Some(metadata) = sheet.rows.iter().find(|record| record.index == *row) {
            if let Some(height) = metadata.height {
                body.push_str(&format!(" ht=\"{}\"", format_number(height)));
                if metadata.custom_height {
                    body.push_str(" customHeight=\"1\"");
                }
            }
            if metadata.hidden {
                body.push_str(" hidden=\"1\"");
            }
        }
        body.push('>');
        let mut sorted = cells.clone();
        sorted.sort_by_key(|cell| cell.column);
        for cell in sorted {
            body.push_str(&cell_xml(cell, table));
        }
        body.push_str("</row>");
    }
    body.push_str("</sheetData>");

    if !sheet.merged.is_empty() {
        body.push_str(&format!("<mergeCells count=\"{}\">", sheet.merged.len()));
        for range in &sheet.merged {
            body.push_str(&format!(
                "<mergeCell ref=\"{}:{}\"/>",
                crate::a1::format_reference(range.start_row, range.start_column),
                crate::a1::format_reference(range.end_row, range.end_column)
            ));
        }
        body.push_str("</mergeCells>");
    }
    if let Some(filter) = sheet.auto_filter {
        body.push_str(&format!(
            "<autoFilter ref=\"{}:{}\"/>",
            crate::a1::format_reference(filter.start_row, filter.start_column),
            crate::a1::format_reference(filter.end_row, filter.end_column)
        ));
    }
    body.push_str("</worksheet>");
    body
}

fn cell_xml(cell: &XlsxCell, table: &StyleTable) -> String {
    if matches!(cell.value, CellValue::Empty) && cell.formula.is_none() && cell.style == CellStyle::default() {
        return String::new();
    }
    let style_attribute = table
        .xf_index(&cell.style)
        .map(|index| format!(" s=\"{index}\""))
        .unwrap_or_default();
    let reference = escape_attribute(&cell.reference);

    let mut body = format!("<c r=\"{reference}\"{style_attribute}");
    if let Some(formula) = &cell.formula {
        let trimmed = formula.source.trim();
        let source = trimmed.strip_prefix('=').unwrap_or(trimmed);
        let cached = formula.cached.as_deref().or(Some(&cell.value));
        match cached.and_then(cached_value) {
            Some((kind, value)) => {
                body.push_str(kind);
                body.push('>');
                if source.is_empty() {
                    body.push_str(&format!("<v>{}</v>", escape_text(&value)));
                }
                else {
                    body.push_str(&format!("<f>{}</f><v>{}</v>", escape_text(source), escape_text(&value)));
                }
            }
            None => {
                if source.is_empty() {
                    body.push_str("/>");
                    return body;
                }
                body.push('>');
                body.push_str(&format!("<f>{}</f>", escape_text(source)));
            }
        }
    }
    else {
        match &cell.value {
            CellValue::Empty => {
                body.push_str("/>");
                return body;
            }
            CellValue::Text(text) | CellValue::SharedString { text, .. } => {
                body.push_str(" t=\"inlineStr\"><is><t xml:space=\"preserve\">");
                body.push_str(&escape_text(text));
                body.push_str("</t></is>");
            }
            value => match cached_value(value) {
                Some((kind, value)) => {
                    body.push_str(kind);
                    body.push('>');
                    body.push_str(&format!("<v>{}</v>", escape_text(&value)));
                }
                None => body.push_str("/>"),
            },
        }
    }
    body.push_str("</c>");
    body
}

/// Maps a stored value to the `t` attribute and the `<v>` payload.
fn cached_value(value: &CellValue) -> Option<(&'static str, String)> {
    match value {
        CellValue::Empty => None,
        CellValue::Number(number) => Some(("", format_number(*number))),
        CellValue::Date(date) => Some(("", format_number(date.serial))),
        CellValue::Boolean(value) => Some((" t=\"b\"", if *value { "1".to_owned() } else { "0".to_owned() })),
        CellValue::Text(text) | CellValue::SharedString { text, .. } => Some((" t=\"str\"", text.clone())),
        CellValue::Error { code } => Some((" t=\"e\"", code.clone())),
    }
}

/// Style table of one written package.
struct StyleTable {
    numbers: Vec<(u32, String)>,
    fonts: Vec<FontSpec>,
    fills: Vec<String>,
    borders: Vec<BorderSpec>,
    xfs: Vec<Xf>,
    index: HashMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct FontSpec {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    name: Option<String>,
    size: Option<f32>,
    color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct BorderSpec {
    top: Option<(String, Option<String>)>,
    right: Option<(String, Option<String>)>,
    bottom: Option<(String, Option<String>)>,
    left: Option<(String, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Xf {
    font: usize,
    fill: usize,
    border: usize,
    num_fmt: u32,
    horizontal: Option<HorizontalAlign>,
    vertical: Option<VerticalAlign>,
    wrap: bool,
}

impl StyleTable {
    fn build(workbook: &XlsxWorkbook) -> Self {
        let mut table = Self {
            numbers: Vec::new(),
            fonts: vec![FontSpec::default()],
            fills: Vec::new(),
            borders: vec![BorderSpec::default()],
            xfs: vec![Xf::default()],
            index: HashMap::new(),
        };
        for sheet in &workbook.sheets {
            for cell in &sheet.cells {
                table.register(&cell.style, &workbook.styles);
            }
        }
        table
    }

    /// Registers one style and returns its cellXfs index.
    fn register(&mut self, style: &CellStyle, styles: &WorkbookStyles) -> usize {
        if *style == CellStyle::default() {
            return 0;
        }
        let key = style.key();
        if let Some(index) = self.index.get(&key) {
            return *index;
        }
        let resolve = |color: &str| resolve_color(color, styles).unwrap_or_else(|| color.to_owned());
        let font = FontSpec {
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strike: style.strike,
            name: style.font_name.clone(),
            size: style.font_size,
            color: style.font_color.as_deref().map(resolve),
        };
        let font = match self.fonts.iter().position(|existing| existing == &font) {
            Some(index) => index,
            None => {
                self.fonts.push(font);
                self.fonts.len() - 1
            }
        };
        let fill = match &style.fill_color {
            Some(color) => {
                let color = resolve(color);
                match self.fills.iter().position(|existing| existing == &color) {
                    Some(index) => index + 2,
                    None => {
                        self.fills.push(color);
                        self.fills.len() + 1
                    }
                }
            }
            None => 0,
        };
        let border = BorderSpec {
            top: style.borders.top.as_ref().map(|edge| (edge.style.clone(), edge.color.as_deref().map(resolve))),
            right: style.borders.right.as_ref().map(|edge| (edge.style.clone(), edge.color.as_deref().map(resolve))),
            bottom: style.borders.bottom.as_ref().map(|edge| (edge.style.clone(), edge.color.as_deref().map(resolve))),
            left: style.borders.left.as_ref().map(|edge| (edge.style.clone(), edge.color.as_deref().map(resolve))),
        };
        let border = match self.borders.iter().position(|existing| existing == &border) {
            Some(index) => index,
            None => {
                self.borders.push(border);
                self.borders.len() - 1
            }
        };
        let num_fmt = match style.number_format.as_deref() {
            Some(code) if !code.eq_ignore_ascii_case("general") && !code.trim().is_empty() => {
                if style.number_format_id != 0
                    && builtin_number_format(style.number_format_id).is_some_and(|builtin| builtin.eq_ignore_ascii_case(code))
                {
                    style.number_format_id
                }
                else {
                    match self.numbers.iter().find(|(_, existing)| existing == code) {
                        Some((id, _)) => *id,
                        None => {
                            let id = 164 + self.numbers.len() as u32;
                            self.numbers.push((id, code.to_owned()));
                            id
                        }
                    }
                }
            }
            _ => style.number_format_id,
        };
        let xf = Xf {
            font,
            fill,
            border,
            num_fmt,
            horizontal: (style.horizontal != HorizontalAlign::General).then_some(style.horizontal),
            vertical: (style.vertical != VerticalAlign::Bottom).then_some(style.vertical),
            wrap: style.wrap_text,
        };
        let index = match self.xfs.iter().position(|existing| existing == &xf) {
            Some(index) => index,
            None => {
                self.xfs.push(xf);
                self.xfs.len() - 1
            }
        };
        self.index.insert(key, index);
        index
    }

    /// Looks up the cellXfs index of a style already registered during `build`.
    fn xf_index(&self, style: &CellStyle) -> Option<usize> {
        if *style == CellStyle::default() {
            return None;
        }
        self.index.get(&style.key()).copied()
    }

    fn xml(&self) -> String {
        let mut body = String::new();
        body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>");
        body.push_str("<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">");
        if !self.numbers.is_empty() {
            body.push_str(&format!("<numFmts count=\"{}\">", self.numbers.len()));
            for (id, code) in &self.numbers {
                body.push_str(&format!("<numFmt numFmtId=\"{id}\" formatCode=\"{}\"/>", escape_attribute(code)));
            }
            body.push_str("</numFmts>");
        }
        body.push_str(&format!("<fonts count=\"{}\">", self.fonts.len()));
        for font in &self.fonts {
            body.push_str("<font>");
            if font.bold {
                body.push_str("<b/>");
            }
            if font.italic {
                body.push_str("<i/>");
            }
            if font.underline {
                body.push_str("<u/>");
            }
            if font.strike {
                body.push_str("<strike/>");
            }
            if let Some(size) = font.size {
                body.push_str(&format!("<sz val=\"{}\"/>", format_number(f64::from(size))));
            }
            if let Some(color) = &font.color
                && let Some(argb) = argb(color)
            {
                body.push_str(&format!("<color rgb=\"{argb}\"/>"));
            }
            if let Some(name) = &font.name {
                body.push_str(&format!("<name val=\"{}\"/>", escape_attribute(name)));
            }
            body.push_str("</font>");
        }
        body.push_str("</fonts>");
        body.push_str(&format!("<fills count=\"{}\">", self.fills.len() + 2));
        body.push_str("<fill><patternFill patternType=\"none\"/></fill>");
        body.push_str("<fill><patternFill patternType=\"gray125\"/></fill>");
        for fill in &self.fills {
            body.push_str(&format!(
                "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{}\"/><bgColor indexed=\"64\"/></patternFill></fill>",
                argb(fill).unwrap_or_else(|| "FFEEEEEE".to_owned())
            ));
        }
        body.push_str("</fills>");
        body.push_str(&format!("<borders count=\"{}\">", self.borders.len()));
        for border in &self.borders {
            body.push_str("<border>");
            for (tag, edge) in [
                ("left", &border.left),
                ("right", &border.right),
                ("top", &border.top),
                ("bottom", &border.bottom),
            ] {
                match edge {
                    Some((style, color)) => {
                        body.push_str(&format!("<{tag} style=\"{}\">", escape_attribute(style)));
                        match color.as_deref().and_then(argb) {
                            Some(argb) => body.push_str(&format!("<color rgb=\"{argb}\"/>")),
                            None => body.push_str("<color auto=\"1\"/>"),
                        }
                        body.push_str(&format!("</{tag}>"));
                    }
                    None => body.push_str(&format!("<{tag}/>")),
                }
            }
            body.push_str("<diagonal/></border>");
        }
        body.push_str("</borders>");
        body.push_str("<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>");
        body.push_str(&format!("<cellXfs count=\"{}\">", self.xfs.len()));
        for xf in &self.xfs {
            body.push_str(&format!(
                "<xf numFmtId=\"{}\" fontId=\"{}\" fillId=\"{}\" borderId=\"{}\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyNumberFormat=\"1\" applyAlignment=\"1\">",
                xf.num_fmt, xf.font, xf.fill, xf.border
            ));
            if xf.horizontal.is_some() || xf.vertical.is_some() || xf.wrap {
                body.push_str("<alignment");
                if let Some(horizontal) = xf.horizontal {
                    body.push_str(&format!(" horizontal=\"{}\"", horizontal_name(horizontal)));
                }
                if let Some(vertical) = xf.vertical {
                    body.push_str(&format!(" vertical=\"{}\"", vertical_name(vertical)));
                }
                if xf.wrap {
                    body.push_str(" wrapText=\"1\"");
                }
                body.push_str("/>");
            }
            body.push_str("</xf>");
        }
        body.push_str("</cellXfs>");
        body.push_str("<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>");
        body.push_str("</styleSheet>");
        body
    }
}

fn horizontal_name(align: HorizontalAlign) -> &'static str {
    match align {
        HorizontalAlign::General => "general",
        HorizontalAlign::Left => "left",
        HorizontalAlign::Center => "center",
        HorizontalAlign::Right => "right",
        HorizontalAlign::Justify => "justify",
        HorizontalAlign::Distributed => "distributed",
        HorizontalAlign::Fill => "fill",
    }
}

fn vertical_name(align: VerticalAlign) -> &'static str {
    match align {
        VerticalAlign::Top => "top",
        VerticalAlign::Center => "center",
        VerticalAlign::Bottom => "bottom",
        VerticalAlign::Justify => "justify",
        VerticalAlign::Distributed => "distributed",
    }
}

/// Converts a stored color into an eight-digit ARGB literal.
fn argb(color: &str) -> Option<String> {
    let hex = color.trim().trim_start_matches('#');
    match hex.len() {
        6 if hex.chars().all(|character| character.is_ascii_hexdigit()) => Some(format!("FF{}", hex.to_ascii_uppercase())),
        8 if hex.chars().all(|character| character.is_ascii_hexdigit()) => Some(hex.to_ascii_uppercase()),
        _ => None,
    }
}

fn format_number(number: f64) -> String {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    }
    else {
        format!("{number}")
    }
}

fn escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn escape_attribute(text: &str) -> String {
    escape_text(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::xlsx::import_xlsx_bytes;

    fn sample() -> XlsxWorkbook {
        let mut workbook = XlsxWorkbook::new("sample.xlsx");
        let mut sheet = XlsxSheet::new("Data", 0);
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Text("name".to_owned()),
            formula: None,
            style_index: None,
            style: CellStyle { bold: true, ..Default::default() },
        });
        sheet.cells.push(XlsxCell {
            reference: "B1".to_owned(),
            row: 1,
            column: 2,
            value: CellValue::Text("amount".to_owned()),
            formula: None,
            style_index: None,
            style: CellStyle::default(),
        });
        sheet.cells.push(XlsxCell {
            reference: "A2".to_owned(),
            row: 2,
            column: 1,
            value: CellValue::Text("x,y".to_owned()),
            formula: None,
            style_index: None,
            style: CellStyle::default(),
        });
        sheet.cells.push(XlsxCell {
            reference: "B2".to_owned(),
            row: 2,
            column: 2,
            value: CellValue::Number(2.5),
            formula: None,
            style_index: None,
            style: CellStyle::default(),
        });
        sheet.cells.push(XlsxCell {
            reference: "B3".to_owned(),
            row: 3,
            column: 2,
            value: CellValue::Number(5.0),
            formula: Some(crate::import::xlsx::model::CellFormula {
                source: "SUM(B2,2.5)".to_owned(),
                kind: crate::import::xlsx::model::FormulaKind::Normal,
                cached: Some(Box::new(CellValue::Number(5.0))),
            }),
            style_index: None,
            style: CellStyle::default(),
        });
        sheet.merged.push(crate::import::xlsx::model::CellRange { start_row: 1, start_column: 1, end_row: 1, end_column: 2 });
        sheet.columns.push(crate::import::xlsx::model::SheetColumn { min: 1, max: 1, width: 20.0, custom: true, hidden: false });
        sheet.freeze = Some(crate::import::xlsx::model::FreezePane { columns: 1, rows: 1 });
        workbook.sheets.push(sheet);
        workbook
    }

    #[test]
    fn writes_a_package_the_reader_reads_back() {
        let workbook = sample();
        let export = export_xlsx_bytes_report(&workbook).expect("write");
        assert!(export.losses.is_empty());
        let imported = import_xlsx_bytes("roundtrip.xlsx", &export.bytes).expect("read");
        let sheet = imported.sheet("Data").expect("sheet");
        assert_eq!(sheet.cell(1, 1).expect("cell").value, CellValue::Text("name".to_owned()));
        assert_eq!(sheet.cell(2, 2).expect("cell").value, CellValue::Number(2.5));
        assert_eq!(sheet.merged.len(), 1);
        assert_eq!(sheet.freeze, Some(crate::import::xlsx::model::FreezePane { columns: 1, rows: 1 }));
        assert_eq!(sheet.columns[0].width, 20.0);
        let formula = sheet.cell(3, 2).and_then(|cell| cell.formula.as_ref()).expect("formula");
        assert_eq!(formula.source, "SUM(B2,2.5)");
        assert!(imported.styles.cell_formats.iter().any(|style| style.bold));
    }

    #[test]
    fn reports_skipped_constructs() {
        let mut workbook = sample();
        workbook.macro_enabled = true;
        workbook.sheets[0].comments.push(crate::import::xlsx::model::CellComment {
            reference: "A1".to_owned(),
            author: None,
            text: "note".to_owned(),
        });
        let report = export_xlsx_bytes_report(&workbook).expect("write");
        let codes = report.losses.iter().map(|marker| marker.code.as_str()).collect::<Vec<_>>();
        assert!(codes.contains(&LOSS_MACRO));
        assert!(codes.contains(&LOSS_COMMENT));
    }

    #[test]
    fn keeps_font_decoration_borders_and_vertical_through_a_round_trip() {
        let mut workbook = XlsxWorkbook::new("decor.xlsx");
        let mut sheet = XlsxSheet::new("S", 0);
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Text("decorated".to_owned()),
            formula: None,
            style_index: None,
            style: CellStyle {
                underline: true,
                strike: true,
                font_name: Some("Consolas".to_owned()),
                vertical: VerticalAlign::Center,
                borders: crate::import::xlsx::model::BorderEdges {
                    top: Some(crate::import::xlsx::model::BorderEdge { style: "thin".to_owned(), color: Some("#FF0000".to_owned()) }),
                    bottom: Some(crate::import::xlsx::model::BorderEdge { style: "medium".to_owned(), color: None }),
                    ..Default::default()
                },
                ..Default::default()
            },
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("decor.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        let style = &imported.sheet("S").expect("sheet").cell(1, 1).expect("cell").style;
        assert!(style.underline, "underline survives the round trip");
        assert!(style.strike, "strike survives the round trip");
        assert_eq!(style.font_name.as_deref(), Some("Consolas"));
        assert_eq!(style.vertical, VerticalAlign::Center);
        assert_eq!(style.borders.top.as_ref().map(|edge| edge.style.as_str()), Some("thin"));
        assert_eq!(style.borders.top.as_ref().and_then(|edge| edge.color.as_deref()), Some("#FF0000"));
        assert_eq!(style.borders.bottom.as_ref().map(|edge| edge.style.as_str()), Some("medium"));
        assert!(style.borders.right.is_none(), "an unwritten edge stays absent");
    }

    #[test]
    fn keeps_builtin_number_format_ids_through_a_round_trip() {
        let mut workbook = XlsxWorkbook::new("builtin.xlsx");
        let mut sheet = XlsxSheet::new("S", 0);
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Number(1234.5),
            formula: None,
            style_index: None,
            style: CellStyle {
                number_format_id: 3,
                number_format: Some("#,##0".to_owned()),
                ..Default::default()
            },
        });
        sheet.cells.push(XlsxCell {
            reference: "B1".to_owned(),
            row: 1,
            column: 2,
            value: CellValue::Number(1.0),
            formula: None,
            style_index: None,
            style: CellStyle { number_format: Some("0.000".to_owned()), ..Default::default() },
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("builtin.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        let sheet = imported.sheet("S").expect("sheet");
        let builtin = &sheet.cell(1, 1).expect("cell").style;
        assert_eq!(builtin.number_format_id, 3, "a built-in code keeps its built-in id");
        assert_eq!(builtin.number_format.as_deref(), Some("#,##0"));
        let custom = &sheet.cell(1, 2).expect("cell").style;
        assert_eq!(custom.number_format.as_deref(), Some("0.000"));
        assert!(custom.number_format_id >= 164, "a custom code gets a custom id, got {}", custom.number_format_id);
    }

    #[test]
    fn writes_defined_names_back_verbatim() {
        use crate::import::xlsx::model::DefinedName;
        let mut workbook = sample();
        workbook.defined_names.push(DefinedName {
            name: "Sales".to_owned(),
            sheet: Some("Data".to_owned()),
            reference: "Data!$A$1:$B$2".to_owned(),
        });
        workbook.defined_names.push(DefinedName { name: "Total".to_owned(), sheet: None, reference: "Data!$B$3".to_owned() });
        let export = export_xlsx_bytes_report(&workbook).expect("write");
        assert!(export.losses.iter().all(|marker| marker.code != "export.xlsx.defined-name"), "defined names are no longer a loss");
        let imported = import_xlsx_bytes("names.xlsx", &export.bytes).expect("read");
        assert_eq!(imported.defined_names, workbook.defined_names);
    }

    #[test]
    fn rejects_a_workbook_without_sheets() {
        let workbook = XlsxWorkbook::new("empty.xlsx");
        assert!(export_xlsx_bytes(&workbook).is_err());
    }

    #[test]
    fn keeps_the_1904_date_system_through_a_round_trip() {
        let mut workbook = XlsxWorkbook::new("mac.xlsx");
        workbook.date_system = DateSystem::Date1904;
        let mut sheet = XlsxSheet::new("Dates", 0);
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Date(crate::import::xlsx::model::ExcelDate {
                serial: 1.0,
                iso: "1904-01-02".to_owned(),
                kind: crate::import::xlsx::model::DateKind::Date,
            }),
            formula: None,
            style_index: None,
            style: CellStyle { number_format: Some("yyyy-mm-dd".to_owned()), ..Default::default() },
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("mac.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        assert_eq!(imported.date_system, DateSystem::Date1904);
        match &imported.sheet("Dates").expect("sheet").cell(1, 1).expect("date cell").value {
            CellValue::Date(date) => {
                assert_eq!(date.serial, 1.0);
                assert_eq!(date.iso, "1904-01-02");
            }
            other => panic!("expected a date, got {other:?}"),
        }
    }

    #[test]
    fn resolves_theme_colors_when_writing() {
        let mut workbook = XlsxWorkbook::new("theme.xlsx");
        workbook.styles.theme_colors.insert("accent1".to_owned(), "#4472C4".to_owned());
        let mut sheet = XlsxSheet::new("S", 0);
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Text("coloured".to_owned()),
            formula: None,
            style_index: None,
            style: CellStyle {
                font_color: Some("theme:4".to_owned()),
                fill_color: Some("theme:4:-0.25".to_owned()),
                ..Default::default()
            },
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("theme.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        let style = &imported.sheet("S").expect("sheet").cell(1, 1).expect("cell").style;
        assert_eq!(style.font_color.as_deref(), Some("#4472C4"));
        let fill = style.fill_color.clone().expect("fill");
        assert!(fill.starts_with('#') && fill.len() == 7, "expected a resolved hex fill, got {fill}");
        assert_ne!(fill, "#4472C4");
    }

    #[test]
    fn keeps_a_custom_default_column_width() {
        let mut workbook = XlsxWorkbook::new("width.xlsx");
        let mut sheet = XlsxSheet::new("S", 0);
        sheet.default_column_width = 12.5;
        sheet.cells.push(XlsxCell {
            reference: "A1".to_owned(),
            row: 1,
            column: 1,
            value: CellValue::Number(1.0),
            formula: None,
            style_index: None,
            style: CellStyle::default(),
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("width.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        assert_eq!(imported.sheet("S").expect("sheet").default_column_width, 12.5);
    }

    #[test]
    fn writes_only_the_cached_value_for_a_shared_child_without_source() {
        let mut workbook = XlsxWorkbook::new("shared.xlsx");
        let mut sheet = XlsxSheet::new("S", 0);
        sheet.cells.push(XlsxCell {
            reference: "B3".to_owned(),
            row: 3,
            column: 2,
            value: CellValue::Number(7.0),
            formula: Some(crate::import::xlsx::model::CellFormula {
                source: String::new(),
                kind: crate::import::xlsx::model::FormulaKind::SharedChild { index: 0 },
                cached: None,
            }),
            style_index: None,
            style: CellStyle::default(),
        });
        workbook.sheets.push(sheet);
        let imported = import_xlsx_bytes("shared.xlsx", &export_xlsx_bytes(&workbook).expect("write")).expect("read");
        let cell = imported.sheet("S").expect("sheet").cell(3, 2).expect("cell");
        assert_eq!(cell.value, CellValue::Number(7.0));
        assert!(cell.formula.is_none(), "an empty shared child must not emit a formula element");
    }
}
