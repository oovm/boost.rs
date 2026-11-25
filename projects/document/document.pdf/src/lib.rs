//! PDF page operations on top of the shared byte-level engine.
//!
//! The object model, parser, stream filters and writer live in
//! `data-binary-pdf`. This crate adds the operations a product exposes:
//! inspection, merge, split, page arrangement, page deletion, rotation and
//! cropping, metadata and bookmark edits, form filling, stamping, one-page
//! creation, and text and image extraction.
//!
//! Every malformed input becomes a [`ServiceError`]; no entry point panics.

#![deny(missing_docs)]

mod compose;
mod create;
mod error;
mod extract;
mod fidelity;
mod links;
mod stamp;

pub use compose::{
    ComposeReport, ComposedSource, PagePlan, PagePlacement, compose, compose_with_title, duplicate_pages, insert_blank_pages,
    insert_pages, interleave_pages, move_pages, replace_pages, reverse_pages, swap_pages,
};
pub use fidelity::{FidelityCode, FidelityLoss};
pub use links::{LinkKind, LinkRecord, NamedDestination, OutlineEntry, extract_links, list_destinations, list_outline};

// The byte-level engine lives in `data-binary-pdf`; the module aliases keep the
// page-operation modules reading as if they owned it.
pub(crate) use data_binary_pdf::{filters, object, parse, writer};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use boost_fs::{OverwritePolicy, PublishOptions, publish_bytes_with_options};
use serde::{Deserialize, Serialize};

use crate::error::{Result, argument, exists_error_at, read_error_at, write_error_at};
use crate::object::{Dict, PdfObject, encode_text};
use crate::parse::{PdfDocument, decode_text_string, parse_document, resolve_object};
use crate::writer::{
    PdfWriter, copy_info, copy_info_fields, copy_object, copy_page, copy_page_with_rotation, media_box_object,
};

pub use crate::error::ServiceError;
pub use crate::error::{ErrorParams, ParamError, codes, params};

/// Document metadata as reported by [`inspect`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfMetadata {
    /// `/Title`, or `null` when the document has none.
    pub title: Option<String>,
    /// `/Author`, or `null`.
    pub author: Option<String>,
    /// `/Subject`, or `null`.
    pub subject: Option<String>,
    /// `/Keywords`, or `null`.
    pub keywords: Option<String>,
    /// `/Creator`, or `null`.
    pub creator: Option<String>,
    /// `/Producer`, or `null`.
    pub producer: Option<String>,
    /// `/CreationDate` in its original PDF text form.
    pub creation_date: Option<String>,
    /// `/ModDate` in its original PDF text form.
    pub mod_date: Option<String>,
}

/// One page's geometry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfPageSize {
    /// Page width in points.
    pub width_pt: f64,
    /// Page height in points.
    pub height_pt: f64,
    /// Effective `/Rotate` value normalized to `0..360`.
    pub rotation: i64,
}

/// One outline entry.
#[derive(Debug, Clone, Serialize)]
pub struct PdfBookmark {
    /// Outline title.
    pub title: String,
    /// Nesting level, top-level entries are `1`.
    pub depth: usize,
}

/// One terminal AcroForm field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfFormField {
    /// Fully qualified field name built from the `/T` chain, such as
    /// `person.name`.
    pub name: String,
    /// Inherited `/FT` value such as `Tx`, `Btn`, `Ch` or `Sig`.
    #[serde(rename = "type")]
    pub field_type: Option<String>,
    /// `/V` as a text or name value, or `null`.
    pub value: Option<String>,
    /// Bit 1 of the inherited `/Ff` flags.
    pub read_only: bool,
    /// Bit 2 of the inherited `/Ff` flags.
    pub required: bool,
}

/// Result of [`inspect`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectReport {
    /// File name of the inspected document.
    pub label: String,
    /// PDF header version such as `1.7`.
    pub version: String,
    /// Number of pages in reading order.
    pub pages: usize,
    /// Always `false`: encrypted documents are rejected before inspection.
    pub encrypted: bool,
    /// Document information dictionary.
    pub metadata: PdfMetadata,
    /// Effective geometry of every page.
    pub page_sizes: Vec<PdfPageSize>,
    /// Flat outline list with nesting depth.
    pub bookmarks: Vec<PdfBookmark>,
    /// Terminal AcroForm fields, empty when the document has no form.
    pub form_fields: Vec<PdfFormField>,
    /// Non-fatal structure problems such as an unreadable form field.
    pub warnings: Vec<String>,
}

/// One image reached through a page's `/Resources /XObject` graph.
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedImage {
    /// One-based page number.
    pub page: usize,
    /// One-based image index within the page.
    pub index: usize,
    /// Resource name used by the page or Form XObject.
    pub name: String,
    /// Pixel width, `0` when the dictionary is unreadable.
    pub width: u32,
    /// Pixel height, `0` when the dictionary is unreadable.
    pub height: u32,
    /// Filter chain such as `DCTDecode` or `FlateDecode`, or `none`.
    pub filter: String,
    /// Written file path; empty when nothing was written.
    pub path: String,
    /// Why the image was skipped or only partially decoded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss: Option<String>,
}

/// Result of [`merge`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeReport {
    /// Output path.
    pub path: String,
    /// Total pages written.
    pub pages: usize,
    /// Output size in bytes.
    pub bytes: usize,
    /// Objects the rebuilt catalog and dropped pages left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// One file produced by [`split`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitPart {
    /// Output path.
    pub path: String,
    /// Pages contained in this file.
    pub pages: usize,
    /// Objects the rebuilt catalog and the pages this part leaves out lost,
    /// never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// Result of [`split`].
#[derive(Debug, Clone, Serialize)]
pub struct SplitReport {
    /// One entry per requested range, in input order.
    pub documents: Vec<SplitPart>,
}

/// Request accepted by [`create`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    /// Output file path.
    pub out: PathBuf,
    /// Optional `/Title` metadata.
    #[serde(default)]
    pub title: Option<String>,
    /// One string per page; `\n` starts a new line inside the page.
    #[serde(default)]
    pub pages: Vec<String>,
    /// Replace the output when it already exists.
    #[serde(default)]
    pub overwrite: bool,
    /// Named paper size (`a3`, `a4`, `a5`, `letter`, `legal`, `tabloid`) or
    /// `WIDTHxHEIGHT` in points. Defaults to Letter (`612x792`).
    #[serde(default)]
    pub page_size: Option<String>,
    /// Base-14 font family: `helvetica`, `helvetica-bold`, `times`,
    /// `times-bold`, `courier` or `courier-bold`. Defaults to `helvetica`.
    #[serde(default)]
    pub font: Option<String>,
    /// Body font size in points, `6..144`. Defaults to `12`.
    #[serde(default)]
    pub font_size: Option<f64>,
    /// Page margin in points. Defaults to `72`.
    #[serde(default)]
    pub margin_pt: Option<f64>,
    /// Horizontal alignment: `left`, `center` or `right`. Defaults to `left`.
    #[serde(default)]
    pub align: Option<String>,
    /// Optional `/Author` metadata.
    #[serde(default)]
    pub author: Option<String>,
    /// Optional `/Subject` metadata.
    #[serde(default)]
    pub subject: Option<String>,
    /// Optional `/Keywords` metadata.
    #[serde(default)]
    pub keywords: Option<String>,
    /// Optional `/Creator` metadata.
    #[serde(default)]
    pub creator: Option<String>,
    /// Draw a `page / total` footer on every page.
    #[serde(default)]
    pub page_numbers: Option<bool>,
}

/// Text stamp applied by [`stamp`].
///
/// The text may contain `{page}` and `{pages}` placeholders, which are
/// replaced with the one-based page number and the document page count.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StampRequest {
    /// Text to paint; `{page}` and `{pages}` are substituted.
    pub text: String,
    /// `center` (default), `top`/`header` or `bottom`/`footer`.
    #[serde(default)]
    pub position: Option<String>,
    /// Base-14 font family, same names as [`CreateRequest::font`].
    #[serde(default)]
    pub font: Option<String>,
    /// Stamp font size in points. Defaults to `36` for `center` and `12` for
    /// headers and footers.
    #[serde(default)]
    pub font_size: Option<f64>,
    /// Clockwise rotation in degrees for a centered stamp, `-180..180`.
    #[serde(default)]
    pub rotation: Option<f64>,
    /// Opacity `0..1`. Defaults to `0.3` for `center` and `0.6` otherwise.
    #[serde(default)]
    pub opacity: Option<f64>,
    /// `#RGB` or `#RRGGBB` text colour. Defaults to mid-grey.
    #[serde(default)]
    pub color: Option<String>,
    /// 1-based page ranges such as `1-3,5`; every page when absent.
    #[serde(default)]
    pub pages: Option<String>,
}

/// Result of [`stamp`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StampReport {
    /// Output path.
    pub path: String,
    /// Pages copied into the output.
    pub pages: usize,
    /// Pages that received the stamp.
    pub pages_stamped: usize,
    /// Output size in bytes.
    pub bytes: usize,
}

/// Result of [`create`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReport {
    /// Output path.
    pub path: String,
    /// Pages written.
    pub pages: usize,
    /// Output size in bytes.
    pub bytes: usize,
}

/// Page reordering, removal and rotation accepted by [`arrange`].
///
/// All page numbers are 1-based and refer to the input document. When both
/// `remove` and `order` are present, removal happens first and `order` must be
/// a permutation of the surviving pages.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentArrange {
    /// Output page sequence, one input page number per output page.
    #[serde(default)]
    pub order: Option<Vec<usize>>,
    /// Clockwise rotation in degrees added to every page's current `/Rotate`.
    /// Only `90`, `180` and `270` are accepted.
    #[serde(default)]
    pub rotate: Option<i32>,
    /// Input pages to drop before applying `order`.
    #[serde(default)]
    pub remove: Option<Vec<usize>>,
}

/// Result of [`arrange`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrangeReport {
    /// Output path.
    pub path: String,
    /// Pages written.
    pub pages: usize,
    /// Objects the rebuilt catalog and dropped pages left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// Result of [`delete_pages`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePagesReport {
    /// Output path.
    pub path: String,
    /// Pages written to the output.
    pub pages: usize,
    /// Pages dropped from the source document.
    pub removed: usize,
    /// Output size in bytes.
    pub bytes: usize,
    /// Objects the rebuilt catalog and removed pages left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// Result of [`rotate_pages`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RotatePagesReport {
    /// Output path.
    pub path: String,
    /// Pages written to the output.
    pub pages: usize,
    /// Pages whose effective `/Rotate` changed.
    pub rotated: usize,
    /// Output size in bytes.
    pub bytes: usize,
    /// Objects the rebuilt catalog left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// Result of [`set_page_boxes`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPageBoxesReport {
    /// Output path.
    pub path: String,
    /// Pages written to the output.
    pub pages: usize,
    /// Pages that received the new `/CropBox`.
    pub cropped: usize,
    /// Output size in bytes.
    pub bytes: usize,
    /// Objects the rebuilt catalog left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// `/Info` fields accepted by [`set_metadata`].
///
/// A missing field keeps its source value, while an empty string removes it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataPatch {
    /// `/Title` replacement.
    #[serde(default)]
    pub title: Option<String>,
    /// `/Author` replacement.
    #[serde(default)]
    pub author: Option<String>,
    /// `/Subject` replacement.
    #[serde(default)]
    pub subject: Option<String>,
    /// `/Keywords` replacement.
    #[serde(default)]
    pub keywords: Option<String>,
    /// `/Creator` replacement.
    #[serde(default)]
    pub creator: Option<String>,
    /// `/Producer` replacement.
    #[serde(default)]
    pub producer: Option<String>,
}

/// Result of [`set_metadata`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMetadataReport {
    /// Output path.
    pub path: String,
    /// Pages copied into the output.
    pub pages: usize,
    /// Metadata of the written file, read back from disk.
    pub metadata: PdfMetadata,
}

/// One bookmark written by [`set_bookmarks`].
#[derive(Debug, Clone, Deserialize)]
pub struct Bookmark {
    /// Outline title; must contain a non-whitespace character.
    pub title: String,
    /// One-based destination page, inside `1..=pages`.
    pub page: u32,
    /// Zero-based nesting level, where `0` is a top-level entry. Skipped levels
    /// are attached to the deepest open entry instead of failing.
    #[serde(default)]
    pub depth: Option<u32>,
}

/// Result of [`set_bookmarks`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkReport {
    /// Output path.
    pub path: String,
    /// Pages copied into the output.
    pub pages: u32,
    /// Outline entries written.
    pub bookmarks: u32,
}

/// One value accepted by [`fill_form`].
///
/// Text fills a text or choice field; a boolean turns a checkbox or radio
/// button on (`true`) or off (`false`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FormValue {
    /// Text written to a text or choice field.
    Text(String),
    /// On/off state written to a button field.
    Toggle(bool),
}

/// Result of [`fill_form`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormFillReport {
    /// Output path.
    pub path: String,
    /// Requested names that were written, in sorted request order.
    pub filled: Vec<String>,
    /// Requested names that matched no terminal field, in sorted order.
    pub missing: Vec<String>,
    /// Pages copied into the output.
    pub pages: usize,
}

/// One text fragment that could not be extracted.
#[derive(Debug, Clone, Serialize)]
pub struct ExtractLoss {
    /// Stable loss code.
    pub code: String,
    /// One-based page number.
    pub page: usize,
    /// Human-readable explanation.
    pub message: String,
}

/// Result of [`extract_text`].
#[derive(Debug, Clone, Serialize)]
pub struct ExtractReport {
    /// Page count of the source document.
    pub pages: usize,
    /// Extracted text with pages separated by blank lines.
    pub markdown: String,
    /// Everything the extractor skipped, never silent.
    pub losses: Vec<ExtractLoss>,
}

/// Reads a PDF and reports its version, page geometry, metadata and outlines.
pub fn inspect(path: &Path) -> Result<InspectReport> {
    let bytes = fs::read(path)
        .map_err(|error| read_error_at(path, format!("{:?}", error.kind()), format!("cannot read {}: {error}", path.display())))?;
    let mut report = inspect_bytes(&bytes)?;
    report.label = label_of(path);
    Ok(report)
}

/// Reports the same facts as [`inspect`] from bytes the caller already owns.
pub fn inspect_bytes(bytes: &[u8]) -> Result<InspectReport> {
    let document = parse_document(bytes)?;
    let metadata = read_metadata(&document)?;
    let page_sizes = document
        .pages
        .iter()
        .map(|page| PdfPageSize {
            width_pt: page.media_box[2] - page.media_box[0],
            height_pt: page.media_box[3] - page.media_box[1],
            rotation: page.rotation,
        })
        .collect();
    let bookmarks = document
        .bookmarks
        .iter()
        .map(|bookmark| PdfBookmark { title: bookmark.title.clone(), depth: bookmark.depth })
        .collect();
    let form_fields = document
        .form_fields
        .iter()
        .map(|field| PdfFormField {
            name: field.name.clone(),
            field_type: field.field_type.clone(),
            value: field.value.clone(),
            read_only: field.read_only,
            required: field.required,
        })
        .collect();
    Ok(InspectReport {
        label: "<memory>".to_string(),
        version: document.version.clone(),
        pages: document.pages.len(),
        encrypted: false,
        metadata,
        page_sizes,
        bookmarks,
        form_fields,
        warnings: document.warnings.clone(),
    })
}

/// Concatenates the pages of every input, in order.
///
/// The `/Info` dictionary comes from the first input; `title` replaces its
/// `/Title` when provided.
pub fn merge(inputs: &[PathBuf], out: &Path, overwrite: bool, title: Option<&str>) -> Result<MergeReport> {
    if inputs.is_empty() {
        return Err(argument("merge needs at least one input file"));
    }
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }

    let mut documents = Vec::with_capacity(inputs.len());
    for path in inputs {
        documents.push(load_document(path)?);
    }

    let mut writer = PdfWriter::new();
    let mut pages = Vec::new();
    let mut info = None;
    for (index, document) in documents.iter().enumerate() {
        let mut map = BTreeMap::new();
        for page in &document.pages {
            pages.push(copy_page(&mut writer, document, &mut map, page)?);
        }
        if index == 0 {
            info = copy_info(&mut writer, document, &mut map, title)?;
        }
    }

    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, overwrite)?;
    let fidelity = documents
        .iter()
        .enumerate()
        .flat_map(|(index, document)| fidelity::losses(index, document, &all_pages(document.pages.len())))
        .collect();
    Ok(MergeReport { path: out.display().to_string(), pages: pages.len(), bytes: bytes.len(), fidelity })
}

/// Splits one PDF into files selected by a 1-based range list such as
/// `1-5,10,15-20`.
pub fn split(path: &Path, ranges: &str, out_dir: &Path) -> Result<SplitReport> {
    let document = load_document(path)?;
    let ranges = parse_ranges(ranges, document.pages.len())?;
    prepare_out_dir(out_dir)?;
    let stem = document_stem(path);
    let mut documents = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        documents.push(write_part(&document, out_dir, &stem, start, end)?);
    }
    Ok(SplitReport { documents })
}

/// Splits one PDF into consecutive files of at most `every` pages each.
pub fn split_every(path: &Path, every: usize, out_dir: &Path) -> Result<SplitReport> {
    if every == 0 {
        return Err(argument("split --every needs at least one page per file"));
    }
    let document = load_document(path)?;
    prepare_out_dir(out_dir)?;
    let stem = document_stem(path);
    let total = document.pages.len();
    let mut documents = Vec::new();
    let mut start = 1usize;
    while start <= total {
        let end = start.saturating_add(every - 1).min(total);
        documents.push(write_part(&document, out_dir, &stem, start, end)?);
        start = end + 1;
    }
    Ok(SplitReport { documents })
}

/// Reorders, removes and rotates pages into a new classic PDF.
///
/// Removal runs before ordering, so `order` must list exactly the surviving
/// input pages. Rotation is added to each page's current `/Rotate` and
/// normalized to `0..360`; a result of `0` omits `/Rotate`. The output file is
/// replaced when it already exists.
pub fn arrange(input: &Path, out: &Path, ops: &DocumentArrange) -> Result<ArrangeReport> {
    let document = load_document(input)?;
    let plan = arrange_plan(document.pages.len(), ops)?;

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(plan.len());
    for number in &plan {
        let page = &document.pages[number - 1];
        let rotation = ops.rotate.map(|delta| normalize_rotation(page.rotation + i64::from(delta)));
        pages.push(copy_page_with_rotation(&mut writer, &document, &mut map, page, rotation)?);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, true)?;
    let kept = plan.iter().copied().collect::<BTreeSet<_>>();
    Ok(ArrangeReport {
        path: out.display().to_string(),
        pages: pages.len(),
        fidelity: fidelity::losses(0, &document, &kept),
    })
}

/// Removes the pages selected by a 1-based range list such as `2,5-7`.
///
/// The range syntax is the one [`split`] accepts. Removing every page is
/// rejected with `session/argument-invalid`, and the output file is replaced
/// only when `overwrite` is true.
pub fn delete_pages(input: &Path, pages: &str, out: &Path, overwrite: bool) -> Result<DeletePagesReport> {
    let document = load_document(input)?;
    let selected = selected_pages(pages, document.pages.len())?;
    if selected.len() == document.pages.len() {
        return Err(argument("delete_pages cannot remove every page"));
    }
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut kept = Vec::with_capacity(document.pages.len() - selected.len());
    for (index, page) in document.pages.iter().enumerate() {
        if selected.contains(&(index + 1)) {
            continue;
        }
        kept.push(copy_page(&mut writer, &document, &mut map, page)?);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &kept, info)?;
    write_file(out, &bytes, overwrite)?;
    let kept_pages = (1..=document.pages.len()).filter(|number| !selected.contains(number)).collect::<BTreeSet<_>>();
    Ok(DeletePagesReport {
        path: out.display().to_string(),
        pages: kept.len(),
        removed: selected.len(),
        bytes: bytes.len(),
        fidelity: fidelity::losses(0, &document, &kept_pages),
    })
}

/// Adds the requested clockwise rotation to the pages selected by a 1-based
/// range list such as `1,3-5`.
///
/// `degrees` must be `90`, `180` or `270` and is added to each page's current
/// `/Rotate`, normalized to `0..360`; a result of `0` omits `/Rotate`. Other
/// pages keep their existing rotation. The output file is replaced only when
/// `overwrite` is true.
pub fn rotate_pages(input: &Path, pages: &str, degrees: i32, out: &Path, overwrite: bool) -> Result<RotatePagesReport> {
    if !matches!(degrees, 90 | 180 | 270) {
        return Err(argument(format!("rotate_pages rotation {degrees} is not 90, 180 or 270")));
    }
    let document = load_document(input)?;
    let selected = selected_pages(pages, document.pages.len())?;
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut written = Vec::with_capacity(document.pages.len());
    let mut rotated = 0;
    for (index, page) in document.pages.iter().enumerate() {
        let rotation = if selected.contains(&(index + 1)) {
            rotated += 1;
            Some(normalize_rotation(page.rotation + i64::from(degrees)))
        } else {
            None
        };
        written.push(copy_page_with_rotation(&mut writer, &document, &mut map, page, rotation)?);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &written, info)?;
    write_file(out, &bytes, overwrite)?;
    Ok(RotatePagesReport {
        path: out.display().to_string(),
        pages: written.len(),
        rotated,
        bytes: bytes.len(),
        fidelity: fidelity::losses(0, &document, &all_pages(document.pages.len())),
    })
}

/// Writes `crop` as `/CropBox` on the pages selected by a 1-based range list
/// such as `1-3`.
///
/// The box corners are `[llx, lly, urx, ury]` in points and must satisfy
/// `urx > llx`, `ury > lly` and stay inside the page's `/MediaBox`. Only
/// `/CropBox` changes; `/MediaBox` and every other page entry are copied
/// unchanged. The output file is replaced only when `overwrite` is true.
pub fn set_page_boxes(input: &Path, pages: &str, crop: [f64; 4], out: &Path, overwrite: bool) -> Result<SetPageBoxesReport> {
    let document = load_document(input)?;
    let selected = selected_pages(pages, document.pages.len())?;
    validate_crop_box(crop, &document, &selected)?;
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut written = Vec::with_capacity(document.pages.len());
    let mut cropped = 0;
    for (index, page) in document.pages.iter().enumerate() {
        let number = copy_page(&mut writer, &document, &mut map, page)?;
        if selected.contains(&(index + 1)) {
            if let Some(PdfObject::Dictionary(dict)) = writer.get_mut(number) {
                dict.insert("CropBox".to_string(), media_box_object(crop));
            }
            cropped += 1;
        }
        written.push(number);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &written, info)?;
    write_file(out, &bytes, overwrite)?;
    Ok(SetPageBoxesReport {
        path: out.display().to_string(),
        pages: written.len(),
        cropped,
        bytes: bytes.len(),
        fidelity: fidelity::losses(0, &document, &all_pages(document.pages.len())),
    })
}

/// The 1-based page numbers of a document that keeps every page.
fn all_pages(page_count: usize) -> BTreeSet<usize> {
    (1..=page_count).collect()
}

/// Expands a 1-based range list into the selected page numbers.
fn selected_pages(ranges: &str, page_count: usize) -> Result<BTreeSet<usize>> {
    let mut selected = BTreeSet::new();
    for (start, end) in parse_ranges(ranges, page_count)? {
        selected.extend(start..=end);
    }
    Ok(selected)
}

/// The largest gap tolerated when a crop box corner is compared with a
/// media box corner.
const BOX_EPSILON: f64 = 1e-6;

/// Validates that `crop` is a non-empty box inside every selected page.
fn validate_crop_box(crop: [f64; 4], document: &PdfDocument, selected: &BTreeSet<usize>) -> Result<()> {
    if !crop.iter().all(|value| value.is_finite()) {
        return Err(argument("crop box corners must be finite numbers"));
    }
    let [llx, lly, urx, ury] = crop;
    if urx - llx <= BOX_EPSILON || ury - lly <= BOX_EPSILON {
        return Err(argument(format!("crop box [{llx} {lly} {urx} {ury}] needs urx > llx and ury > lly")));
    }
    for number in selected {
        let media = document.pages[number - 1].media_box;
        if llx < media[0] - BOX_EPSILON
            || lly < media[1] - BOX_EPSILON
            || urx > media[2] + BOX_EPSILON
            || ury > media[3] + BOX_EPSILON
        {
            return Err(argument(format!(
                "crop box [{llx} {lly} {urx} {ury}] is outside the media box [{} {} {} {}] of page {number}",
                media[0], media[1], media[2], media[3]
            )));
        }
    }
    Ok(())
}

/// Copies every page into a new PDF while rewriting its information
/// dictionary.
///
/// Fields absent from `patch` keep their source value and an empty string
/// removes the field. The output file is replaced when it already exists.
pub fn set_metadata(input: &Path, out: &Path, patch: &MetadataPatch) -> Result<SetMetadataReport> {
    let document = load_document(input)?;

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(document.pages.len());
    for page in &document.pages {
        pages.push(copy_page(&mut writer, &document, &mut map, page)?);
    }

    let mut edits: Vec<(&str, Option<&str>)> = Vec::new();
    for (key, value) in [
        ("Title", &patch.title),
        ("Author", &patch.author),
        ("Subject", &patch.subject),
        ("Keywords", &patch.keywords),
        ("Creator", &patch.creator),
        ("Producer", &patch.producer),
    ] {
        if let Some(text) = value {
            edits.push((key, if text.is_empty() { None } else { Some(text.as_str()) }));
        }
    }
    let info = copy_info_fields(&mut writer, &document, &mut map, &edits)?;
    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, true)?;

    let written = parse_document(&bytes)?;
    Ok(SetMetadataReport { path: out.display().to_string(), pages: pages.len(), metadata: read_metadata(&written)? })
}

/// Copies every page into a new PDF while replacing its outline tree.
///
/// The written catalog points `/Outlines` at the new entries and sets
/// `/PageMode /UseOutlines`. Any outline tree in the source is replaced;
/// pages, other catalog entries and the information dictionary are kept. The
/// output file is replaced when it already exists.
pub fn set_bookmarks(input: &Path, out: &Path, bookmarks: &[Bookmark]) -> Result<BookmarkReport> {
    let document = load_document(input)?;
    validate_bookmarks(bookmarks, document.pages.len())?;

    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(document.pages.len());
    for page in &document.pages {
        pages.push(copy_page(&mut writer, &document, &mut map, page)?);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;

    let outlines = write_outlines(&mut writer, &pages, bookmarks);
    let mut catalog_entries = Dict::new();
    catalog_entries.insert("Outlines".to_string(), PdfObject::Reference(outlines, 0));
    catalog_entries.insert("PageMode".to_string(), PdfObject::Name("UseOutlines".to_string()));
    let bytes = finish_document_with(&mut writer, &pages, info, catalog_entries)?;
    write_file(out, &bytes, true)?;

    Ok(BookmarkReport {
        path: out.display().to_string(),
        pages: pages.len() as u32,
        bookmarks: bookmarks.len() as u32,
    })
}

/// Copies every page into a new PDF while filling AcroForm field values.
///
/// Values are keyed by the fully qualified field name reported by
/// [`InspectReport::form_fields`]. Text fields receive a PDF string, choice
/// fields use a `/Opt` name when the value matches an option, and button
/// fields get the `/AP` on-state name or `/Off`. Unknown names are returned in
/// [`FormFillReport::missing`] instead of failing. The new catalog sets
/// `/AcroForm /NeedAppearances true` so readers repaint the widgets.
pub fn fill_form(input: &Path, out: &Path, values: &BTreeMap<String, FormValue>) -> Result<FormFillReport> {
    if values.is_empty() {
        return Err(argument("fill_form needs at least one field value"));
    }
    if values.keys().any(|name| name.trim().is_empty()) {
        return Err(argument("fill_form field names must not be empty"));
    }

    let document = load_document(input)?;
    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::with_capacity(document.pages.len());
    for page in &document.pages {
        pages.push(copy_page(&mut writer, &document, &mut map, page)?);
    }
    let info = copy_info(&mut writer, &document, &mut map, None)?;

    let mut matched = BTreeSet::new();
    let mut catalog_entries = Dict::new();
    if let Some(form) = document
        .catalog
        .get("AcroForm")
        .and_then(|value| resolve_object(&document.objects, value).ok())
        .and_then(PdfObject::as_dict)
        .cloned()
    {
        let mut copied = Dict::new();
        for (key, value) in &form {
            if key != "Fields" {
                copied.insert(key.clone(), copy_object(&mut writer, &document, &mut map, value)?);
            }
        }
        let mut fields = Vec::new();
        if let Some(value) = form.get("Fields")
            && let Ok(PdfObject::Array(items)) = resolve_object(&document.objects, value)
        {
            let mut visited = BTreeSet::new();
            for item in items {
                fields.push(write_form_field(
                    &mut writer,
                    &document,
                    &mut map,
                    item,
                    "",
                    None,
                    values,
                    &mut matched,
                    &mut visited,
                    0,
                )?);
            }
        }
        copied.insert("Fields".to_string(), PdfObject::Array(fields));
        copied.insert("NeedAppearances".to_string(), PdfObject::Boolean(true));
        let number = writer.add(PdfObject::Dictionary(copied));
        catalog_entries.insert("AcroForm".to_string(), PdfObject::Reference(number, 0));
    }

    let bytes = finish_document_with(&mut writer, &pages, info, catalog_entries)?;
    write_file(out, &bytes, true)?;

    let mut filled = Vec::new();
    let mut missing = Vec::new();
    for name in values.keys() {
        if matched.contains(name) { filled.push(name.clone()) } else { missing.push(name.clone()) }
    }
    Ok(FormFillReport { path: out.display().to_string(), filled, missing, pages: pages.len() })
}

/// The deepest AcroForm tree level [`fill_form`] follows.
const MAX_FORM_DEPTH: usize = 32;

/// Copies one field subtree, applying `values` to terminal fields.
#[allow(clippy::too_many_arguments)]
fn write_form_field(
    writer: &mut PdfWriter,
    source: &PdfDocument,
    map: &mut BTreeMap<u32, u32>,
    node: &PdfObject,
    prefix: &str,
    inherited_type: Option<&str>,
    values: &BTreeMap<String, FormValue>,
    matched: &mut BTreeSet<String>,
    visited: &mut BTreeSet<u32>,
    depth: usize,
) -> Result<PdfObject> {
    if depth > MAX_FORM_DEPTH {
        return copy_object(writer, source, map, node);
    }
    if let PdfObject::Reference(number, _) = node
        && !visited.insert(*number)
    {
        return copy_object(writer, source, map, node);
    }
    let dict = match resolve_object(&source.objects, node) {
        Ok(PdfObject::Dictionary(dict)) => dict.clone(),
        _ => return copy_object(writer, source, map, node),
    };

    let segment = dict.get("T").and_then(|value| form_text(&source.objects, value)).unwrap_or_default();
    let name = if segment.is_empty() {
        prefix.to_string()
    } else if prefix.is_empty() {
        segment
    } else {
        format!("{prefix}.{segment}")
    };
    let field_type = match dict.get("FT") {
        Some(value) => resolve_object(&source.objects, value)
            .ok()
            .and_then(|object| object.as_name().map(str::to_string))
            .or_else(|| inherited_type.map(str::to_string)),
        None => inherited_type.map(str::to_string),
    };
    let kids: Vec<PdfObject> = match dict.get("Kids") {
        Some(value) => match resolve_object(&source.objects, value) {
            Ok(PdfObject::Array(items)) => items.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };

    if !kids.is_empty() {
        let mut copied = Dict::new();
        for (key, value) in &dict {
            if key != "Kids" {
                copied.insert(key.clone(), copy_object(writer, source, map, value)?);
            }
        }
        let mut children = Vec::with_capacity(kids.len());
        for kid in &kids {
            children.push(write_form_field(
                writer,
                source,
                map,
                kid,
                &name,
                field_type.as_deref(),
                values,
                matched,
                visited,
                depth + 1,
            )?);
        }
        copied.insert("Kids".to_string(), PdfObject::Array(children));
        let number = writer.add(PdfObject::Dictionary(copied));
        return Ok(PdfObject::Reference(number, 0));
    }

    let new_value = values.get(&name).map(|value| form_value_object(&source.objects, &dict, value));
    if new_value.is_some() {
        matched.insert(name);
    }

    if let PdfObject::Reference(_, _) = node {
        let copied = copy_object(writer, source, map, node)?;
        if let (PdfObject::Reference(target, _), Some(value)) = (&copied, new_value)
            && let Some(PdfObject::Dictionary(stored)) = writer.get_mut(*target)
        {
            stored.insert("V".to_string(), value);
        }
        return Ok(copied);
    }

    let mut copied = match copy_object(writer, source, map, node)? {
        PdfObject::Dictionary(dict) => dict,
        other => return Ok(other),
    };
    if let Some(value) = new_value {
        copied.insert("V".to_string(), value);
    }
    let number = writer.add(PdfObject::Dictionary(copied));
    Ok(PdfObject::Reference(number, 0))
}

/// Builds the `/V` object written for one terminal field.
fn form_value_object(objects: &BTreeMap<u32, PdfObject>, dict: &Dict, value: &FormValue) -> PdfObject {
    match value {
        FormValue::Toggle(state) => {
            if *state {
                PdfObject::Name(on_state_name(objects, dict).unwrap_or_else(|| "Yes".to_string()))
            } else {
                PdfObject::Name("Off".to_string())
            }
        }
        FormValue::Text(text) => match dict.get("Opt").and_then(|opt| choice_value(objects, opt, text)) {
            Some(value) => value,
            None => PdfObject::String(encode_text(text)),
        },
    }
}

/// Returns the `/V` object for a choice value that matches an `/Opt` entry.
fn choice_value(objects: &BTreeMap<u32, PdfObject>, opt: &PdfObject, text: &str) -> Option<PdfObject> {
    let items = resolve_object(objects, opt).ok()?.as_array()?;
    for item in items {
        match resolve_object(objects, item) {
            Ok(PdfObject::Name(name)) if name == text => return Some(PdfObject::Name(name.clone())),
            Ok(PdfObject::String(bytes)) if decode_text_string(bytes) == text => {
                return Some(PdfObject::String(encode_text(text)));
            }
            _ => {}
        }
    }
    None
}

/// Returns the checked state name advertised by a button's `/AP /N` states.
fn on_state_name(objects: &BTreeMap<u32, PdfObject>, dict: &Dict) -> Option<String> {
    let appearance = resolve_object(objects, dict.get("AP")?).ok()?.as_dict()?;
    let normal = resolve_object(objects, appearance.get("N")?).ok()?;
    normal.as_dict()?.keys().find(|name| name.as_str() != "Off").cloned()
}

/// Reads a text or name value used by the form tree.
fn form_text(objects: &BTreeMap<u32, PdfObject>, value: &PdfObject) -> Option<String> {
    match resolve_object(objects, value).ok()? {
        PdfObject::String(bytes) => Some(decode_text_string(bytes)),
        PdfObject::Name(name) => Some(name.clone()),
        _ => None,
    }
}

/// The deepest outline level accepted by [`set_bookmarks`].
const MAX_BOOKMARK_DEPTH: u32 = 63;

fn validate_bookmarks(bookmarks: &[Bookmark], page_count: usize) -> Result<()> {
    if bookmarks.is_empty() {
        return Err(argument("set_bookmarks needs at least one bookmark"));
    }
    for (index, bookmark) in bookmarks.iter().enumerate() {
        if bookmark.title.trim().is_empty() {
            return Err(argument(format!("bookmark {} has an empty title", index + 1)));
        }
        if bookmark.page == 0 || bookmark.page as usize > page_count {
            return Err(argument(format!(
                "bookmark {} targets page {} but the document has {page_count} pages",
                index + 1,
                bookmark.page
            )));
        }
        if bookmark.depth.unwrap_or(0) > MAX_BOOKMARK_DEPTH {
            return Err(argument(format!("bookmark {} depth must be at most {MAX_BOOKMARK_DEPTH}", index + 1)));
        }
    }
    Ok(())
}

/// Writes the outline tree and returns the `/Outlines` object number.
fn write_outlines(writer: &mut PdfWriter, pages: &[u32], bookmarks: &[Bookmark]) -> u32 {
    let root = writer.reserve();
    let mut numbers = Vec::with_capacity(bookmarks.len());
    for _ in bookmarks {
        numbers.push(writer.reserve());
    }

    // `stack` holds the open ancestors: index `0` is the outline root, index
    // `n + 1` the entry opened at depth `n`.
    let mut parents = Vec::with_capacity(bookmarks.len());
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut stack = vec![root];
    for (index, bookmark) in bookmarks.iter().enumerate() {
        let depth = (bookmark.depth.unwrap_or(0) as usize).min(stack.len() - 1);
        stack.truncate(depth + 1);
        let parent = stack[stack.len() - 1];
        parents.push(parent);
        children.entry(parent).or_default().push(numbers[index]);
        stack.push(numbers[index]);
    }

    for (index, bookmark) in bookmarks.iter().enumerate() {
        let number = numbers[index];
        let parent = parents[index];
        let siblings = &children[&parent];
        let position = siblings.iter().position(|entry| *entry == number).expect("an entry is its parent's child");
        let mut dict = Dict::new();
        dict.insert("Title".to_string(), PdfObject::String(encode_text(&bookmark.title)));
        dict.insert("Parent".to_string(), PdfObject::Reference(parent, 0));
        if position > 0 {
            dict.insert("Prev".to_string(), PdfObject::Reference(siblings[position - 1], 0));
        }
        if let Some(next) = siblings.get(position + 1) {
            dict.insert("Next".to_string(), PdfObject::Reference(*next, 0));
        }
        let page = pages[bookmark.page as usize - 1];
        dict.insert(
            "Dest".to_string(),
            PdfObject::Array(vec![PdfObject::Reference(page, 0), PdfObject::Name("Fit".to_string())]),
        );
        if let Some(kids) = children.get(&number) {
            dict.insert("First".to_string(), PdfObject::Reference(kids[0], 0));
            dict.insert("Last".to_string(), PdfObject::Reference(*kids.last().expect("children are never empty"), 0));
            dict.insert("Count".to_string(), PdfObject::Integer(descendants(number, &children) as i64));
        }
        writer.set(number, PdfObject::Dictionary(dict));
    }

    let top_level = &children[&root];
    let mut dict = Dict::new();
    dict.insert("Type".to_string(), PdfObject::Name("Outlines".to_string()));
    dict.insert("First".to_string(), PdfObject::Reference(top_level[0], 0));
    dict.insert(
        "Last".to_string(),
        PdfObject::Reference(*top_level.last().expect("the outline has at least one entry"), 0),
    );
    dict.insert("Count".to_string(), PdfObject::Integer(bookmarks.len() as i64));
    writer.set(root, PdfObject::Dictionary(dict));
    root
}

fn descendants(item: u32, children: &BTreeMap<u32, Vec<u32>>) -> usize {
    children
        .get(&item)
        .map(|kids| kids.len() + kids.iter().map(|kid| descendants(*kid, children)).sum::<usize>())
        .unwrap_or(0)
}

/// Validates the 1-based indices of [`DocumentArrange`] and returns the input
/// page numbers in output order.
fn arrange_plan(page_count: usize, ops: &DocumentArrange) -> Result<Vec<usize>> {
    if let Some(rotate) = ops.rotate
        && !matches!(rotate, 90 | 180 | 270)
    {
        return Err(argument(format!("arrange rotation {rotate} is not 90, 180 or 270")));
    }
    if let Some(order) = &ops.order
        && order.is_empty()
    {
        return Err(argument("arrange --order must name at least one page"));
    }

    let mut removed = BTreeSet::new();
    for number in ops.remove.as_deref().unwrap_or(&[]) {
        check_page_number(*number, page_count)?;
        if !removed.insert(*number) {
            return Err(argument(format!("page {number} appears twice in --remove")));
        }
    }
    let survivors: Vec<usize> = (1..=page_count).filter(|number| !removed.contains(number)).collect();
    if survivors.is_empty() {
        return Err(argument("arrange cannot remove every page"));
    }

    let Some(order) = &ops.order else {
        return Ok(survivors);
    };
    let mut seen = BTreeSet::new();
    for number in order {
        check_page_number(*number, page_count)?;
        if removed.contains(number) {
            return Err(argument(format!("page {number} is removed but still present in --order")));
        }
        if !seen.insert(*number) {
            return Err(argument(format!("page {number} appears twice in --order")));
        }
    }
    if order.len() != survivors.len() {
        return Err(argument("--order must list every surviving page exactly once"));
    }
    Ok(order.clone())
}

pub(crate) fn check_page_number(number: usize, page_count: usize) -> Result<()> {
    if number == 0 || number > page_count {
        return Err(argument(format!("page {number} is outside the {page_count} available pages")));
    }
    Ok(())
}

fn normalize_rotation(value: i64) -> i64 {
    let wrapped = value % 360;
    if wrapped < 0 { wrapped + 360 } else { wrapped }
}

fn prepare_out_dir(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir).map_err(|error| write_error_at(out_dir, format!("cannot create {}: {error}", out_dir.display())))
}

fn document_stem(path: &Path) -> String {
    path.file_stem().and_then(|value| value.to_str()).unwrap_or("document").to_string()
}

fn write_part(document: &PdfDocument, out_dir: &Path, stem: &str, start: usize, end: usize) -> Result<SplitPart> {
    let mut writer = PdfWriter::new();
    let mut map = BTreeMap::new();
    let mut pages = Vec::new();
    for page in document.pages.iter().take(end).skip(start - 1) {
        pages.push(copy_page(&mut writer, document, &mut map, page)?);
    }
    let info = copy_info(&mut writer, document, &mut map, None)?;
    let bytes = finish_document(&mut writer, &pages, info)?;

    let name = if start == end { format!("{stem}-p{start}.pdf") } else { format!("{stem}-p{start}-{end}.pdf") };
    let target = out_dir.join(name);
    write_file(&target, &bytes, true)?;
    let kept = (start..=end).collect::<BTreeSet<_>>();
    Ok(SplitPart { path: target.display().to_string(), pages: pages.len(), fidelity: fidelity::losses(0, document, &kept) })
}

/// Creates a new PDF with one text page per request entry, paginating long
/// pages and applying the requested geometry, font, alignment and metadata.
pub fn create(request: &CreateRequest) -> Result<CreateReport> {
    if request.pages.is_empty() {
        return Err(argument("create needs at least one page"));
    }
    if request.out.exists() && !request.overwrite {
        return Err(exists_error_at(&request.out, format!("{} already exists", request.out.display())));
    }
    let outcome = create::build(request)?;
    write_file(&request.out, &outcome.bytes, request.overwrite)?;
    Ok(CreateReport { path: request.out.display().to_string(), pages: outcome.pages, bytes: outcome.bytes.len() })
}

/// Paints a text stamp over the selected pages and writes a new PDF.
///
/// The output replaces an existing file. The stamp is appended after the
/// original content in an isolated graphics state, so it never changes the
/// page's existing drawing operators.
pub fn stamp(input: &Path, out: &Path, request: &StampRequest) -> Result<StampReport> {
    stamp::stamp(input, out, request)
}

/// Extracts text from content streams decoded through their filters.
///
/// Flate, LZW, ASCIIHex, ASCII85 and RunLength streams are decoded. Image
/// codecs are reported in [`ExtractReport::losses`] instead of being
/// approximated.
pub fn extract_text(path: &Path) -> Result<ExtractReport> {
    let bytes = fs::read(path)
        .map_err(|error| read_error_at(path, format!("{:?}", error.kind()), format!("cannot read {}: {error}", path.display())))?;
    extract_text_bytes(&bytes)
}

/// Extracts text from bytes the caller already owns.
pub fn extract_text_bytes(bytes: &[u8]) -> Result<ExtractReport> {
    let document = parse_document(bytes)?;
    let extraction = extract::extract(&document)?;
    Ok(ExtractReport {
        pages: document.pages.len(),
        markdown: extraction.markdown,
        losses: extraction
            .losses
            .into_iter()
            .map(|loss| ExtractLoss { code: loss.code, page: loss.page, message: loss.message })
            .collect(),
    })
}

/// Extracts images from every page's `/Resources /XObject` graph into
/// `out_dir`.
///
/// JPEG payloads are written unchanged, 8-bit DeviceGray/DeviceRGB Flate
/// images become PNG (with `/SMask` alpha where readable), and every image
/// that cannot be decoded is returned with a `loss` explanation instead of a
/// guessed file.
pub fn extract_images(path: &Path, out_dir: &Path) -> Result<Vec<ExtractedImage>> {
    let document = load_document(path)?;
    let stem = document_stem(path);
    extract::extract_images(&document, out_dir, &stem)
}

/// Extracts images from bytes the caller already owns, naming files after `stem`.
pub fn extract_images_bytes(bytes: &[u8], out_dir: &Path, stem: &str) -> Result<Vec<ExtractedImage>> {
    let document = parse_document(bytes)?;
    extract::extract_images(&document, out_dir, stem)
}

pub(crate) fn load_document(path: &Path) -> Result<PdfDocument> {
    let bytes = fs::read(path)
        .map_err(|error| read_error_at(path, format!("{:?}", error.kind()), format!("cannot read {}: {error}", path.display())))?;
    parse_document(&bytes)
}

fn label_of(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn read_metadata(document: &PdfDocument) -> Result<PdfMetadata> {
    let dict = match document.info {
        Some(number) => document.objects.get(&number).and_then(PdfObject::as_dict),
        None => None,
    };
    Ok(PdfMetadata {
        title: information_field(document, dict, "Title")?,
        author: information_field(document, dict, "Author")?,
        subject: information_field(document, dict, "Subject")?,
        keywords: information_field(document, dict, "Keywords")?,
        creator: information_field(document, dict, "Creator")?,
        producer: information_field(document, dict, "Producer")?,
        creation_date: information_field(document, dict, "CreationDate")?,
        mod_date: information_field(document, dict, "ModDate")?,
    })
}

fn information_field(document: &PdfDocument, dict: Option<&Dict>, key: &str) -> Result<Option<String>> {
    let Some(value) = dict.and_then(|dict| dict.get(key)) else {
        return Ok(None);
    };
    match resolve_object(&document.objects, value)? {
        PdfObject::String(bytes) => Ok(Some(decode_text_string(bytes))),
        PdfObject::Name(name) => Ok(Some(name.clone())),
        _ => Ok(None),
    }
}

pub(crate) fn finish_document(writer: &mut PdfWriter, pages: &[u32], info: Option<u32>) -> Result<Vec<u8>> {
    finish_document_with(writer, pages, info, Dict::new())
}

/// Serializes the document, adding `catalog_entries` to the new catalog.
pub(crate) fn finish_document_with(
    writer: &mut PdfWriter,
    pages: &[u32],
    info: Option<u32>,
    catalog_entries: Dict,
) -> Result<Vec<u8>> {
    let pages_number = writer.reserve();
    let mut pages_tree = Dict::new();
    pages_tree.insert("Type".to_string(), PdfObject::Name("Pages".to_string()));
    pages_tree
        .insert("Kids".to_string(), PdfObject::Array(pages.iter().map(|number| PdfObject::Reference(*number, 0)).collect()));
    pages_tree.insert("Count".to_string(), PdfObject::Integer(pages.len() as i64));
    writer.set(pages_number, PdfObject::Dictionary(pages_tree));

    let mut catalog = Dict::new();
    catalog.insert("Type".to_string(), PdfObject::Name("Catalog".to_string()));
    catalog.insert("Pages".to_string(), PdfObject::Reference(pages_number, 0));
    for (key, value) in catalog_entries {
        catalog.insert(key, value);
    }
    let root = writer.add(PdfObject::Dictionary(catalog));
    writer.finish(root, info)
}

pub(crate) fn parse_ranges(ranges: &str, page_count: usize) -> Result<Vec<(usize, usize)>> {
    let mut parsed = Vec::new();
    for raw in ranges.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            return Err(argument("page ranges contain an empty entry"));
        }
        let (start, end) = match token.split_once('-') {
            Some((start, end)) => (start.trim(), end.trim()),
            None => (token, token),
        };
        let start: usize = start.parse().map_err(|_| argument(format!("invalid page range `{token}`")))?;
        let end: usize = end.parse().map_err(|_| argument(format!("invalid page range `{token}`")))?;
        if start == 0 || end == 0 {
            return Err(argument("page numbers start at 1"));
        }
        if end < start {
            return Err(argument(format!("page range `{token}` is reversed")));
        }
        if end > page_count {
            return Err(argument(format!("page range `{token}` exceeds the {page_count} available pages")));
        }
        parsed.push((start, end));
    }
    if parsed.is_empty() {
        return Err(argument("at least one page range is required"));
    }
    Ok(parsed)
}

pub(crate) fn write_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    if path.exists() && !overwrite {
        return Err(exists_error_at(path, format!("{} already exists", path.display())));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| write_error_at(parent, format!("cannot create {}: {error}", parent.display())))?;
    }
    let options = PublishOptions {
        overwrite: if overwrite { OverwritePolicy::Replace } else { OverwritePolicy::RefuseExisting },
        ..PublishOptions::default()
    };
    publish_bytes_with_options(publish_target(path), bytes, options)
        .map_err(|error| write_error_at(path, format!("cannot write {}: {error}", path.display())))
}

/// Keeps a bare file name anchored to the working directory so the publisher
/// always has a folder to stage in.
fn publish_target(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => path.to_path_buf(),
        _ => PathBuf::from(".").join(path),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Temporary directory helper mirroring the product integration tests.
    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("document-pdf-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&root).expect("create test workspace");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }

        fn create(&self, name: &str, pages: &[&str]) -> PathBuf {
            let out = self.path(name);
            let report = create(&CreateRequest {
                out: out.clone(),
                pages: pages.iter().map(|page| (*page).to_string()).collect(),
                overwrite: true,
                ..CreateRequest::default()
            })
            .expect("create fixture PDF");
            assert_eq!(report.pages, pages.len());
            out
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn crop_values(document: &PdfDocument, index: usize) -> Option<Vec<f64>> {
        let array = document.pages[index].own.get("CropBox")?.as_array()?;
        array
            .iter()
            .map(|item| match item {
                PdfObject::Integer(value) => Some(*value as f64),
                PdfObject::Real(value) => Some(*value),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn delete_pages_keeps_only_the_unselected_pages() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three", "Four"]);
        let out = workspace.path("kept.pdf");

        let report = delete_pages(&source, "2,4", &out, false).expect("delete pages");
        assert_eq!(report.pages, 2);
        assert_eq!(report.removed, 2);
        assert!(report.bytes > 0);
        assert_eq!(inspect(&out).expect("inspect output").pages, 2);

        let text = extract_text(&out).expect("extract output").markdown;
        assert!(text.contains("One"), "{text}");
        assert!(text.contains("Three"), "{text}");
        assert!(!text.contains("Two"), "{text}");
        assert!(!text.contains("Four"), "{text}");
    }

    #[test]
    fn delete_pages_rejects_every_page_bad_ranges_and_existing_output() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two"]);
        let out = workspace.path("out.pdf");

        let error = delete_pages(&source, "1-2", &out, true).expect_err("every page cannot be removed");
        assert_eq!(error.code(), "session/argument-invalid");
        let error = delete_pages(&source, "3", &out, true).expect_err("out of range");
        assert_eq!(error.code(), "session/argument-invalid");
        let error = delete_pages(&source, "1-", &out, true).expect_err("malformed range");
        assert_eq!(error.code(), "session/argument-invalid");

        fs::write(&out, b"placeholder").expect("seed output");
        let error = delete_pages(&source, "1", &out, false).expect_err("existing output");
        assert_eq!(error.code(), "io/exists");
        delete_pages(&source, "1", &out, true).expect("overwrite output");
        assert_eq!(inspect(&out).expect("inspect overwritten").pages, 1);
    }

    #[test]
    fn rotate_pages_changes_only_the_selection_and_normalizes() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three"]);

        let first = workspace.path("rotated.pdf");
        let report = rotate_pages(&source, "1-2", 90, &first, false).expect("rotate pages");
        assert_eq!(report.pages, 3);
        assert_eq!(report.rotated, 2);
        assert!(report.bytes > 0);
        let sizes = inspect(&first).expect("inspect rotated").page_sizes;
        assert_eq!(sizes[0].rotation, 90);
        assert_eq!(sizes[1].rotation, 90);
        assert_eq!(sizes[2].rotation, 0);

        let second = workspace.path("rotated-again.pdf");
        rotate_pages(&first, "1", 180, &second, false).expect("add 180 degrees");
        assert_eq!(inspect(&second).expect("inspect").page_sizes[0].rotation, 270);

        let full = workspace.path("rotated-full.pdf");
        rotate_pages(&second, "1", 90, &full, false).expect("complete the turn");
        assert_eq!(inspect(&full).expect("inspect").page_sizes[0].rotation, 0);
        let document = parse_document(&fs::read(&full).expect("read output")).expect("parse output");
        assert!(document.pages[0].own.get("Rotate").is_none(), "a zero rotation must be omitted");
    }

    #[test]
    fn rotate_pages_rejects_unsupported_degrees() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One"]);
        let out = workspace.path("out.pdf");

        for degrees in [0, 45, -90, 360] {
            let error = rotate_pages(&source, "1", degrees, &out, true).expect_err("unsupported rotation");
            assert_eq!(error.code(), "session/argument-invalid", "{degrees}");
        }
    }

    #[test]
    fn set_page_boxes_writes_crop_box_and_keeps_media_box() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two"]);
        let out = workspace.path("cropped.pdf");

        let report = set_page_boxes(&source, "1", [10.0, 20.0, 300.0, 400.0], &out, false).expect("set page boxes");
        assert_eq!(report.pages, 2);
        assert_eq!(report.cropped, 1);
        assert!(report.bytes > 0);

        let document = parse_document(&fs::read(&out).expect("read output")).expect("parse output");
        assert_eq!(crop_values(&document, 0), Some(vec![10.0, 20.0, 300.0, 400.0]));
        assert!(crop_values(&document, 1).is_none());
        assert_eq!(document.pages[0].media_box, [0.0, 0.0, 612.0, 792.0]);
        let sizes = inspect(&out).expect("inspect cropped").page_sizes;
        assert_eq!(sizes[0].width_pt, 612.0);
        assert_eq!(sizes[0].height_pt, 792.0);
    }

    #[test]
    fn set_page_boxes_rejects_invalid_and_out_of_bounds_boxes() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One"]);
        let out = workspace.path("out.pdf");

        let cases = [
            [300.0, 20.0, 10.0, 400.0],
            [10.0, 20.0, 10.0, 400.0],
            [0.0, 0.0, 700.0, 400.0],
            [-10.0, 0.0, 300.0, 400.0],
            [f64::NAN, 0.0, 300.0, 400.0],
        ];
        for crop in cases {
            let error = set_page_boxes(&source, "1", crop, &out, true).expect_err("invalid crop box");
            assert_eq!(error.code(), "session/argument-invalid", "{crop:?}");
        }
    }
}
