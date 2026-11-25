//! Find-and-replace over page content streams.
//!
//! A PDF text run is a string operand of a text-showing operator, addressed by
//! the glyph codes of whichever font `/Tf` selected. The engine has no font
//! program, so a rewrite is only attempted when that font uses a standard
//! single-byte encoding and every character of the new text has a PDFDocEncoding
//! byte. Everything the rewrite cannot address — a composite font's glyph ids, a
//! `/Differences` remap, a match split across show operations, text inside a
//! Form XObject — is reported as a [`TextReplaceLoss`] instead of being dropped
//! silently.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use crate::{
    PageTextReplacement, ReplaceTextRequest, TextReplaceCode, TextReplaceLoss, TextReplaceReport,
    error::{Result, argument, invalid},
    filters, finish_document, load_document,
    object::{Dict, PdfObject, PdfStream},
    parse::{Parser, PdfDocument, PdfPage, decode_text_string, encode_doc_char, is_delimiter, is_whitespace, resolve_object},
    parse_ranges, write_file,
    writer::{PdfWriter, copy_info, copy_page},
};

/// Replaces every occurrence of `find` on the selected pages and writes the
/// rebuilt document to `out`.
pub(crate) fn replace(input: &Path, out: &Path, request: &ReplaceTextRequest, overwrite: bool) -> Result<TextReplaceReport> {
    let find = request.find.as_str();
    if find.is_empty() {
        return Err(argument("find text must not be empty"));
    }
    if request.replace.chars().any(|character| encode_doc_char(character).is_none()) {
        return Err(argument("replacement text has no single-byte PDFDocEncoding form"));
    }

    let document = load_document(input)?;
    let total = document.pages.len();
    let selected = selection(request.pages.as_deref(), total)?;

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(total);
    let mut replacements = Vec::new();
    let mut losses = Vec::new();
    let mut form_pages = Vec::new();
    let mut applied = BTreeSet::new();

    for (index, page) in document.pages.iter().enumerate() {
        let number = index + 1;
        let page_number = copy_page(&mut writer, &document, &mut map, page)?;
        pages.push(page_number);
        if !selected.contains(&number) {
            continue;
        }
        let outcome = replace_page(&document, page, number, page_number, &mut writer, find, &request.replace, &mut applied)?;
        losses.extend(outcome.losses);
        if outcome.replaced > 0 {
            replacements.push(PageTextReplacement { page: number, replaced: outcome.replaced });
        }
        if outcome.has_forms {
            form_pages.push(number);
        }
    }

    let replaced: usize = replacements.iter().map(|page| page.replaced).sum();
    if replaced == 0 {
        // Nothing matched anywhere. If the pages that failed to match carry a
        // Form XObject, that graph is the likeliest home of the text: say so
        // rather than letting the caller believe the document has no match.
        for page in form_pages {
            losses.push(TextReplaceLoss {
                page,
                code: TextReplaceCode::TextInFormXObject,
                message: "text was not replaced because the page draws it from a Form XObject, which this rewriter does not scan".to_string(),
            });
        }
    }

    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, overwrite)?;
    Ok(TextReplaceReport {
        path: out.display().to_string(),
        pages: total,
        pages_changed: replacements.len(),
        replaced,
        replacements,
        losses,
        bytes: bytes.len(),
    })
}

/// Resolves the 1-based pages a request scopes the rewrite to.
fn selection(ranges: Option<&str>, total: usize) -> Result<BTreeSet<usize>> {
    let Some(ranges) = ranges.map(str::trim).filter(|value| !value.is_empty())
    else {
        return Ok((1..=total).collect());
    };
    let mut selected = BTreeSet::new();
    for (start, end) in parse_ranges(ranges, total)? {
        for page in start..=end {
            selected.insert(page);
        }
    }
    Ok(selected)
}

struct PageOutcome {
    replaced: usize,
    losses: Vec<TextReplaceLoss>,
    has_forms: bool,
}

struct StreamRewrite {
    bytes: Vec<u8>,
    occurrences: usize,
}

#[allow(clippy::too_many_arguments)]
fn replace_page(
    document: &PdfDocument,
    page: &PdfPage,
    number: usize,
    page_number: u32,
    writer: &mut PdfWriter,
    find: &str,
    replace_with: &str,
    applied: &mut BTreeSet<u32>,
) -> Result<PageOutcome> {
    let mut outcome = PageOutcome { replaced: 0, losses: Vec::new(), has_forms: false };
    let resources =
        page.inherited.get("Resources").and_then(|value| resolve_object(&document.objects, value).ok()).and_then(PdfObject::as_dict);
    outcome.has_forms = has_form_xobject(document, resources);

    let streams = content_streams(document, page)?;
    let mut rewrites = Vec::with_capacity(streams.len());
    let mut text = String::new();
    let mut matched = 0usize;

    for stream in &streams {
        let bytes = match filters::decode(stream, &document.objects) {
            Ok(bytes) => bytes,
            Err(error) => {
                let code = if error.code() == "format/unsupported" {
                    TextReplaceCode::ContentFilterUnsupported
                }
                else {
                    TextReplaceCode::ContentFilterInvalid
                };
                outcome.losses.push(TextReplaceLoss { page: number, code, message: error.message().to_string() });
                rewrites.push(None);
                continue;
            }
        };
        let scan = scan_stream(&bytes, resources, document, find, replace_with);
        text.push_str(&scan.text);
        matched += scan.matched;
        if let Some(message) = scan.parse_error {
            outcome.losses.push(TextReplaceLoss { page: number, code: TextReplaceCode::ContentStreamUnreadable, message });
        }
        if scan.skipped > 0 {
            outcome.losses.push(TextReplaceLoss {
                page: number,
                code: TextReplaceCode::FontEncodingUnsupported,
                message: format!(
                    "{} occurrence(s) sit in {}; the rewriter only addresses standard single-byte encodings",
                    scan.skipped,
                    scan.skipped_fonts.into_iter().collect::<Vec<_>>().join(", ")
                ),
            });
        }
        if scan.unencodable > 0 {
            outcome.losses.push(TextReplaceLoss {
                page: number,
                code: TextReplaceCode::TextEncodingUnsupported,
                message: format!(
                    "{} occurrence(s) share a string with characters that have no single-byte form, so the run was left untouched",
                    scan.unencodable
                ),
            });
        }
        if scan.edits.is_empty() {
            rewrites.push(None);
        }
        else {
            rewrites.push(Some(StreamRewrite { bytes: apply_edits(&bytes, &scan.edits), occurrences: scan.occurrences }));
        }
    }

    if matched == 0 && text.contains(find) {
        outcome.losses.push(TextReplaceLoss {
            page: number,
            code: TextReplaceCode::TextSplitAcrossRuns,
            message: "the match is split across several text-showing operations and cannot be rewritten in place".to_string(),
        });
    }

    patch_contents(writer, page_number, &rewrites, applied, &mut outcome.replaced)?;
    Ok(outcome)
}

/// Resolves the streams `/Contents` names, in drawing order.
fn content_streams<'a>(document: &'a PdfDocument, page: &'a PdfPage) -> Result<Vec<&'a PdfStream>> {
    let Some(value) = page.own.get("Contents")
    else {
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

/// Replaces the copied `/Contents` streams with their rewritten bytes.
///
/// The rewritten text is written uncompressed: the stream dictionary is reset,
/// which drops `/Filter` and `/DecodeParms` with the old bytes. Occurrences are
/// counted once per copied stream object, so a stream shared by two pages is
/// not reported twice.
fn patch_contents(
    writer: &mut PdfWriter,
    page_number: u32,
    rewrites: &[Option<StreamRewrite>],
    applied: &mut BTreeSet<u32>,
    replaced: &mut usize,
) -> Result<()> {
    let Some(contents) = writer.get(page_number).and_then(PdfObject::as_dict).and_then(|dict| dict.get("Contents").cloned())
    else {
        return Ok(());
    };
    match contents {
        PdfObject::Reference(number, _) => match writer.get(number).cloned() {
            Some(PdfObject::Array(items)) => {
                let mut updated = items;
                for (index, rewrite) in rewrites.iter().enumerate() {
                    let Some(slot) = updated.get_mut(index)
                    else {
                        continue;
                    };
                    if let Some(rewrite) = rewrite {
                        *slot = rewrite_value(writer, slot, rewrite, applied, replaced);
                    }
                }
                if let Some(PdfObject::Array(slot)) = writer.get_mut(number) {
                    *slot = updated;
                }
            }
            Some(PdfObject::Stream(_)) => {
                if let Some(Some(rewrite)) = rewrites.first() {
                    rewrite_value(writer, &PdfObject::Reference(number, 0), rewrite, applied, replaced);
                }
            }
            _ => return Err(invalid("/Contents points at neither a stream nor an array of streams")),
        },
        PdfObject::Array(items) => {
            let mut updated = items;
            for (index, rewrite) in rewrites.iter().enumerate() {
                let Some(slot) = updated.get_mut(index)
                else {
                    continue;
                };
                if let Some(rewrite) = rewrite {
                    *slot = rewrite_value(writer, slot, rewrite, applied, replaced);
                }
            }
            insert_contents(writer, page_number, PdfObject::Array(updated));
        }
        PdfObject::Stream(_) => {
            if let Some(Some(rewrite)) = rewrites.first() {
                *replaced += rewrite.occurrences;
                insert_contents(writer, page_number, PdfObject::Stream(PdfStream { dict: Dict::new(), data: rewrite.bytes.clone() }));
            }
        }
        PdfObject::Null => {}
        _ => return Err(invalid("/Contents is neither a stream nor an array of streams")),
    }
    Ok(())
}

fn insert_contents(writer: &mut PdfWriter, page_number: u32, contents: PdfObject) {
    if let Some(PdfObject::Dictionary(dict)) = writer.get_mut(page_number) {
        dict.insert("Contents".to_string(), contents);
    }
}

fn rewrite_value(
    writer: &mut PdfWriter,
    value: &PdfObject,
    rewrite: &StreamRewrite,
    applied: &mut BTreeSet<u32>,
    replaced: &mut usize,
) -> PdfObject {
    match value {
        PdfObject::Reference(number, _) => {
            if applied.insert(*number)
                && let Some(PdfObject::Stream(stream)) = writer.get_mut(*number)
            {
                stream.dict = Dict::new();
                stream.data = rewrite.bytes.clone();
                *replaced += rewrite.occurrences;
            }
            value.clone()
        }
        PdfObject::Stream(_) => {
            *replaced += rewrite.occurrences;
            PdfObject::Stream(PdfStream { dict: Dict::new(), data: rewrite.bytes.clone() })
        }
        other => other.clone(),
    }
}

enum Token {
    Object(PdfObject),
    Operator(String),
}

struct Operand {
    object: PdfObject,
    start: usize,
    end: usize,
}

struct Edit {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Scan {
    edits: Vec<Edit>,
    text: String,
    occurrences: usize,
    matched: usize,
    skipped: usize,
    skipped_fonts: BTreeSet<String>,
    unencodable: usize,
    parse_error: Option<String>,
}

fn scan_stream(bytes: &[u8], resources: Option<&Dict>, document: &PdfDocument, find: &str, replace_with: &str) -> Scan {
    let mut scan = Scan::default();
    let mut parser = Parser::new(bytes);
    let mut operands: Vec<Operand> = Vec::new();
    let mut font: Option<String> = None;

    while let Some(token) = next_token(&mut parser) {
        let (token, start, end) = match token {
            Ok(token) => token,
            Err(error) => {
                scan.parse_error = Some(error.message().to_string());
                break;
            }
        };
        match token {
            Token::Object(object) => operands.push(Operand { object, start, end }),
            Token::Operator(operator) => {
                match operator.as_str() {
                    "Tf" => {
                        if let Some(Operand { object: PdfObject::Name(name), .. }) = operands.first() {
                            font = Some(name.clone());
                        }
                    }
                    "Tj" | "'" | "\"" => {
                        if let Some(operand) = operands.last()
                            && let PdfObject::String(value) = &operand.object
                        {
                            scan_string(&mut scan, value, operand.start, operand.end, font.as_deref(), resources, document, find, replace_with);
                        }
                    }
                    "TJ" => {
                        if let Some(Operand { object: PdfObject::Array(_), start: array_start, .. }) = operands.last() {
                            for span in string_spans(bytes, *array_start) {
                                let Some(value) = string_at(bytes, span.0)
                                else {
                                    continue;
                                };
                                scan_string(&mut scan, &value, span.0, span.1, font.as_deref(), resources, document, find, replace_with);
                            }
                        }
                    }
                    "BI" => skip_inline_image(&mut parser),
                    _ => {}
                }
                operands.clear();
            }
        }
    }
    scan
}

#[allow(clippy::too_many_arguments)]
fn scan_string(
    scan: &mut Scan,
    value: &[u8],
    start: usize,
    end: usize,
    font: Option<&str>,
    resources: Option<&Dict>,
    document: &PdfDocument,
    find: &str,
    replace_with: &str,
) {
    let decoded = decode_text_string(value);
    scan.text.push_str(&decoded);
    if !decoded.contains(find) {
        return;
    }
    scan.matched += 1;
    let occurrences = decoded.matches(find).count();
    match font_safety(document, resources, font) {
        FontSafety::Safe => {
            let updated = decoded.replace(find, replace_with);
            match encode_literal(&updated) {
                Some(bytes) => {
                    scan.occurrences += occurrences;
                    scan.edits.push(Edit { start, end, bytes });
                }
                None => scan.unencodable += occurrences,
            }
        }
        FontSafety::Unsafe(reason) => {
            scan.skipped += occurrences;
            scan.skipped_fonts.insert(reason);
        }
    }
}

enum FontSafety {
    Safe,
    Unsafe(String),
}

/// Decides whether a rewrite may address glyphs through `font`.
///
/// Composite (Type0) and Type3 fonts address glyph ids rather than characters,
/// and a `/Differences` array remaps codes, so none of them can take a plain
/// byte string without corrupting the run.
fn font_safety(document: &PdfDocument, resources: Option<&Dict>, name: Option<&str>) -> FontSafety {
    let Some(name) = name
    else {
        return FontSafety::Unsafe("no /Tf font was selected".to_string());
    };
    let Some(resources) = resources
    else {
        return FontSafety::Unsafe(format!("font `{name}` has no page resources"));
    };
    let Some(fonts) = resources.get("Font").and_then(|value| resolve_object(&document.objects, value).ok()).and_then(PdfObject::as_dict)
    else {
        return FontSafety::Unsafe(format!("font `{name}` is not in the page resources"));
    };
    let Some(font) = fonts.get(name).and_then(|value| resolve_object(&document.objects, value).ok()).and_then(PdfObject::as_dict)
    else {
        return FontSafety::Unsafe(format!("font `{name}` is not in the page resources"));
    };
    let subtype = font.get("Subtype").and_then(|value| resolve_object(&document.objects, value).ok()).and_then(PdfObject::as_name);
    match subtype {
        Some("Type0") => return FontSafety::Unsafe(format!("font `{name}` is composite (Type0) and uses glyph ids")),
        Some("Type3") => return FontSafety::Unsafe(format!("font `{name}` is Type3 and uses local glyph ids")),
        _ => {}
    }
    let Some(encoding) = font.get("Encoding")
    else {
        return FontSafety::Safe;
    };
    match resolve_object(&document.objects, encoding) {
        Ok(PdfObject::Name(encoding)) => match encoding.as_str() {
            "WinAnsiEncoding" | "MacRomanEncoding" | "StandardEncoding" => FontSafety::Safe,
            other => FontSafety::Unsafe(format!("font `{name}` uses /{other}")),
        },
        Ok(PdfObject::Dictionary(dict)) => {
            if dict.contains_key("Differences") {
                FontSafety::Unsafe(format!("font `{name}` remaps glyphs with /Differences"))
            }
            else {
                FontSafety::Safe
            }
        }
        _ => FontSafety::Unsafe(format!("font `{name}` has an unreadable /Encoding")),
    }
}

fn has_form_xobject(document: &PdfDocument, resources: Option<&Dict>) -> bool {
    let Some(xobjects) =
        resources.and_then(|resources| resources.get("XObject")).and_then(|value| resolve_object(&document.objects, value).ok())
    else {
        return false;
    };
    let PdfObject::Dictionary(xobjects) = xobjects
    else {
        return false;
    };
    xobjects.values().any(|value| {
        matches!(resolve_object(&document.objects, value), Ok(PdfObject::Stream(stream)) if stream.dict.get("Subtype").and_then(PdfObject::as_name) == Some("Form"))
    })
}

fn next_token(parser: &mut Parser<'_>) -> Option<Result<(Token, usize, usize)>> {
    parser.skip_ws();
    let byte = parser.peek()?;
    let start = parser.pos;
    let object_start =
        byte == b'/' || byte == b'(' || byte == b'<' || byte == b'[' || byte == b'+' || byte == b'-' || byte == b'.' || byte.is_ascii_digit();
    if object_start {
        return Some(parser.parse_object(0).map(|object| (Token::Object(object), start, parser.pos)));
    }

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
    Some(Ok((Token::Operator(String::from_utf8_lossy(token).into_owned()), start, parser.pos)))
}

/// Byte spans of the string elements of the `TJ` array that starts at `start`.
fn string_spans(bytes: &[u8], start: usize) -> Vec<(usize, usize)> {
    let mut parser = Parser::new(bytes);
    parser.pos = start;
    if parser.peek() != Some(b'[') {
        return Vec::new();
    }
    parser.bump();
    let mut spans = Vec::new();
    loop {
        parser.skip_ws();
        match parser.peek() {
            None | Some(b']') => break,
            Some(_) => {}
        }
        let element_start = parser.pos;
        match parser.parse_object(0) {
            Ok(object) => {
                if matches!(object, PdfObject::String(_)) {
                    spans.push((element_start, parser.pos));
                }
            }
            Err(_) => break,
        }
    }
    spans
}

fn string_at(bytes: &[u8], start: usize) -> Option<Vec<u8>> {
    let mut parser = Parser::new(bytes);
    parser.pos = start;
    match parser.parse_object(0) {
        Ok(PdfObject::String(value)) => Some(value),
        _ => None,
    }
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

fn apply_edits(bytes: &[u8], edits: &[Edit]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0usize;
    for edit in edits {
        if edit.start < cursor || edit.end > bytes.len() {
            continue;
        }
        output.extend_from_slice(&bytes[cursor..edit.start]);
        output.extend_from_slice(&edit.bytes);
        cursor = edit.end;
    }
    output.extend_from_slice(&bytes[cursor..]);
    output
}

/// Writes a literal string whose bytes all have a PDFDocEncoding form.
///
/// Returns `None` when any character has no single byte, so the caller leaves
/// the original run untouched instead of writing a byte that addresses another
/// glyph.
fn encode_literal(text: &str) -> Option<Vec<u8>> {
    let mut output = String::from("(");
    for character in text.chars() {
        let byte = encode_doc_char(character)?;
        match byte {
            b'(' => output.push_str("\\("),
            b')' => output.push_str("\\)"),
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7E => output.push(byte as char),
            other => output.push_str(&format!("\\{other:03o}")),
        }
    }
    output.push(')');
    Some(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{CreateRequest, create, extract_text};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("document-pdf-text-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&root).expect("create test workspace");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path(name);
            fs::write(&path, bytes).expect("write test PDF");
            path
        }

        fn create(&self, name: &str, pages: &[&str]) -> PathBuf {
            let out = self.path(name);
            create(&CreateRequest {
                out: out.clone(),
                pages: pages.iter().map(|page| (*page).to_string()).collect(),
                overwrite: true,
                ..CreateRequest::default()
            })
            .expect("create fixture PDF");
            out
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Assembles a classic PDF from object bodies numbered from 1.
    fn assemble(objects: &[&str]) -> Vec<u8> {
        let bodies: Vec<Vec<u8>> = objects.iter().map(|body| body.as_bytes().to_vec()).collect();
        assemble_bytes(&bodies)
    }

    /// Assembles a classic PDF from raw object bodies, so a fixture can carry
    /// binary stream data.
    fn assemble_bytes(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut output = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(output.len());
            output.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            output.extend_from_slice(body);
            output.extend_from_slice(b"\nendobj\n");
        }
        let xref = output.len();
        output.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        output.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
        output
    }

    fn request(find: &str, replace: &str) -> ReplaceTextRequest {
        ReplaceTextRequest { find: find.to_string(), replace: replace.to_string(), pages: None }
    }

    #[test]
    fn replaces_every_occurrence_and_round_trips_through_extraction() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["Total 100 and 100 again", "second page"]);
        let out = workspace.path("final.pdf");

        let report = replace(&source, &out, &request("100", "250"), true).expect("replace");
        assert_eq!(report.replaced, 2);
        assert_eq!(report.pages_changed, 1);
        assert_eq!(report.pages, 2);
        assert!(report.losses.is_empty(), "{:?}", report.losses);

        let text = extract_text(&out).expect("extract rewritten text");
        assert!(text.markdown.contains("Total 250 and 250 again"), "{}", text.markdown);
        assert!(text.markdown.contains("second page"));
        assert!(!text.markdown.contains("100"));
    }

    #[test]
    fn scoping_to_a_page_leaves_other_pages_untouched() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["keep 100", "change 100"]);
        let out = workspace.path("scoped.pdf");

        let report =
            replace(&source, &out, &ReplaceTextRequest { pages: Some("2".to_string()), ..request("100", "1") }, true).expect("replace");
        assert_eq!((report.replaced, report.pages_changed), (1, 1));

        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("keep 100"), "{}", text.markdown);
        assert!(text.markdown.contains("change 1"), "{}", text.markdown);
    }

    #[test]
    fn deleting_a_run_writes_an_empty_string() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["a draft copy"]);
        let out = workspace.path("deleted.pdf");

        let report = replace(&source, &out, &request("draft ", ""), true).expect("replace");
        assert_eq!(report.replaced, 1);
        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("a copy"), "{}", text.markdown);
    }

    #[test]
    fn unmatched_text_reports_zero_without_touching_the_page() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["nothing here"]);
        let out = workspace.path("same.pdf");

        let report = replace(&source, &out, &request("absent", "x"), true).expect("replace");
        assert_eq!((report.replaced, report.pages_changed), (0, 0));
        assert!(report.losses.is_empty(), "{:?}", report.losses);
        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("nothing here"));
    }

    #[test]
    fn composite_fonts_report_a_skip_instead_of_corrupting_the_run() {
        let workspace = Workspace::new();
        let source = workspace.write(
            "type0.pdf",
            &assemble(&[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
                "<< /Length 38 >>\nstream\nBT /F1 12 Tf 20 40 Td (Hello) Tj ET\nendstream",
                "<< /Type /Font /Subtype /Type0 /BaseFont /Noto /Encoding /Identity-H /DescendantFonts [] >>",
            ]),
        );
        let out = workspace.path("type0-out.pdf");

        let report = replace(&source, &out, &request("Hello", "World"), true).expect("replace");
        assert_eq!(report.replaced, 0);
        assert!(report.losses.iter().any(|loss| loss.code == TextReplaceCode::FontEncodingUnsupported), "{:?}", report.losses);
        // The original run must survive byte for byte.
        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("Hello"));
    }

    #[test]
    fn a_match_split_across_show_operations_is_reported() {
        let workspace = Workspace::new();
        let source = workspace.write(
            "split.pdf",
            &assemble(&[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
                "<< /Length 44 >>\nstream\nBT /F1 12 Tf 20 40 Td [(Hel) (lo)] TJ ET\nendstream",
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
            ]),
        );
        let out = workspace.path("split-out.pdf");

        let report = replace(&source, &out, &request("Hello", "World"), true).expect("replace");
        assert_eq!(report.replaced, 0);
        assert!(report.losses.iter().any(|loss| loss.code == TextReplaceCode::TextSplitAcrossRuns), "{:?}", report.losses);
    }

    #[test]
    fn compressed_content_streams_are_rewritten_and_written_uncompressed() {
        use data_compression::{DeflateLevel, Zlib};

        let workspace = Workspace::new();
        let content = b"BT /F1 12 Tf 20 40 Td (Report 42) Tj ET";
        let compressed = Zlib::encode(content, DeflateLevel::default());
        let mut stream = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", compressed.len()).into_bytes();
        stream.extend_from_slice(&compressed);
        stream.extend_from_slice(b"\nendstream");

        let source = workspace.write(
            "flate.pdf",
            &assemble_bytes(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
                stream,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
            ]),
        );
        let out = workspace.path("flate-out.pdf");

        let report = replace(&source, &out, &request("Report 42", "Report 7"), true).expect("replace");
        assert_eq!(report.replaced, 1);
        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("Report 7"), "{}", text.markdown);
        assert!(!text.markdown.contains("Report 42"));
    }

    #[test]
    fn literal_escapes_round_trip_through_the_writer() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["path A"]);
        let out = workspace.path("escaped.pdf");

        let report = replace(&source, &out, &request("path", "a(b)c\\d"), true).expect("replace");
        assert_eq!(report.replaced, 1);
        let text = extract_text(&out).expect("extract");
        assert!(text.markdown.contains("a(b)c\\d"), "{}", text.markdown);
    }

    #[test]
    fn malformed_input_fails_without_writing_output() {
        let workspace = Workspace::new();
        let junk = workspace.write("junk.pdf", b"not a PDF");
        let out = workspace.path("never.pdf");

        let error = replace(&junk, &out, &request("a", "b"), true).expect_err("junk must fail");
        assert_eq!(error.code(), "format/invalid");
        assert!(!out.exists());
    }

    #[test]
    fn rejecting_a_find_or_replace_that_cannot_be_addressed() {
        let workspace = Workspace::new();
        let source = workspace.create("draft.pdf", &["text"]);
        let out = workspace.path("unused.pdf");

        let error = replace(&source, &out, &request("", "b"), true).expect_err("empty find");
        assert_eq!(error.code(), "session/argument-invalid");
        let error = replace(&source, &out, &request("text", "\u{4E2D}"), true).expect_err("non-Latin replacement");
        assert_eq!(error.code(), "session/argument-invalid");
        assert!(!out.exists());
    }
}
