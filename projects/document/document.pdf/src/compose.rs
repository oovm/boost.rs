//! Cross-document page assembly on top of the shared byte-level engine.
//!
//! [`crate::arrange`] reorders the pages of one input. The operations here
//! take an explicit output plan, so one call can splice pages from several
//! PDFs, repeat a page or insert blank pages without a temporary
//! split-and-merge round trip. Every selection is a typed 1-based list; the
//! range spellings products accept on the command line are parsed by the
//! product before calling in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, argument, exists_error_at};
use crate::fidelity::{self, FidelityLoss};
use crate::object::{Dict, PdfObject, PdfStream};
use crate::writer::{PdfWriter, copy_info, copy_page, media_box_object};
use crate::{DocumentArrange, arrange, check_page_number, finish_document, load_document, write_file};

/// One output page in a [`PagePlan`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PagePlacement {
    /// Copy the 1-based `page` from the 0-based index into [`PagePlan::sources`].
    Source {
        /// Index of the source document in [`PagePlan::sources`].
        source: usize,
        /// 1-based page number within that source.
        page: usize,
    },
    /// Write a blank page of the given size in points.
    Blank {
        /// Page width in points.
        width_pt: f64,
        /// Page height in points.
        height_pt: f64,
    },
}

/// Explicit cross-document output order accepted by [`compose`].
#[derive(Debug, Clone)]
pub struct PagePlan {
    /// Input PDFs referenced by [`PagePlacement::Source`].
    pub sources: Vec<PathBuf>,
    /// Output pages in order; repeats and blanks are legal.
    pub placements: Vec<PagePlacement>,
}

/// One source document as reported by [`compose`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposedSource {
    /// Input path.
    pub path: String,
    /// Pages the source document carries.
    pub pages: usize,
    /// Placements taken from this source.
    pub used: usize,
}

/// Result of [`compose`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeReport {
    /// Output path.
    pub path: String,
    /// Pages written.
    pub pages: usize,
    /// Blank pages written.
    pub blank_pages: usize,
    /// Output size in bytes.
    pub bytes: usize,
    /// Per-source usage, in plan order.
    pub sources: Vec<ComposedSource>,
    /// Objects the rebuilt catalog and unplaced pages left behind, never silent.
    pub fidelity: Vec<FidelityLoss>,
}

/// Writes the pages named by a [`PagePlan`] into one new classic PDF.
///
/// Sources are parsed once and pages are copied through shared object maps,
/// so repeated placements of one page share its objects. A blank page carries
/// a MediaBox, empty resources and an empty content stream. The information
/// dictionary comes from the first source. The output file is replaced only
/// when `overwrite` is true.
pub fn compose(plan: &PagePlan, out: &Path, overwrite: bool) -> Result<ComposeReport> {
    compose_with_title(plan, out, overwrite, None)
}

/// Writes the pages named by a [`PagePlan`] and replaces the first source's
/// `/Title` with `title` when one is given.
///
/// Behaves like [`compose`]; `title` is the only information field callers may
/// override, and the remaining `/Info` entries still come from the first
/// source unchanged. The output file is replaced only when `overwrite` is true.
pub fn compose_with_title(plan: &PagePlan, out: &Path, overwrite: bool, title: Option<&str>) -> Result<ComposeReport> {
    if plan.placements.is_empty() {
        return Err(argument("compose needs at least one page placement"));
    }
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }

    let mut documents = Vec::with_capacity(plan.sources.len());
    for path in &plan.sources {
        documents.push(load_document(path)?);
    }

    let mut writer = PdfWriter::new();
    let mut maps = vec![BTreeMap::new(); documents.len()];
    let mut used = vec![0usize; documents.len()];
    let mut kept: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); documents.len()];
    let mut pages = Vec::with_capacity(plan.placements.len());
    let mut blank_pages = 0usize;
    for placement in &plan.placements {
        match placement {
            PagePlacement::Source { source, page } => {
                let Some(document) = documents.get(*source) else {
                    return Err(argument(format!(
                        "a placement names source {source} of {} sources",
                        plan.sources.len()
                    )));
                };
                check_page_number(*page, document.pages.len())?;
                pages.push(copy_page(&mut writer, document, &mut maps[*source], &document.pages[page - 1])?);
                used[*source] += 1;
                kept[*source].insert(*page);
            }
            PagePlacement::Blank { width_pt, height_pt } => {
                check_blank_size(*width_pt, *height_pt)?;
                pages.push(blank_page(&mut writer, *width_pt, *height_pt));
                blank_pages += 1;
            }
        }
    }

    let mut info = None;
    if let (Some(document), Some(map)) = (documents.first(), maps.first_mut()) {
        info = copy_info(&mut writer, document, map, title)?;
    }
    let bytes = finish_document(&mut writer, &pages, info)?;
    write_file(out, &bytes, overwrite)?;
    let sources = plan
        .sources
        .iter()
        .zip(&documents)
        .zip(&used)
        .map(|((path, document), used)| ComposedSource {
            path: path.display().to_string(),
            pages: document.pages.len(),
            used: *used,
        })
        .collect();
    let fidelity = documents
        .iter()
        .enumerate()
        .flat_map(|(index, document)| fidelity::losses(index, document, &kept[index]))
        .collect();
    Ok(ComposeReport { path: out.display().to_string(), pages: pages.len(), blank_pages, bytes: bytes.len(), sources, fidelity })
}

/// Repeats every selected page directly after itself `times` occurrences total.
///
/// The document keeps its order; `times: 3` writes each selected page three
/// times in a row. The selection must be non-empty, every page must exist and
/// appear once, and `times` is limited to `2..=100`.
pub fn duplicate_pages(input: &Path, selection: &[usize], times: u32, out: &Path, overwrite: bool) -> Result<ComposeReport> {
    if !(2..=100).contains(&times) {
        return Err(argument("duplicate_pages times must be between 2 and 100"));
    }
    if selection.is_empty() {
        return Err(argument("duplicate_pages needs at least one selected page"));
    }
    let total = page_count(input)?;
    check_selection(selection, total)?;
    let selected = BTreeSet::from_iter(selection.iter().copied());
    let mut placements = Vec::with_capacity(total + selected.len() * (times as usize - 1));
    for page in 1..=total {
        placements.push(PagePlacement::Source { source: 0, page });
        if selected.contains(&page) {
            placements.extend(std::iter::repeat_n(PagePlacement::Source { source: 0, page }, times as usize - 1));
        }
    }
    compose(&PagePlan { sources: vec![input.to_path_buf()], placements }, out, overwrite)
}

/// Inserts the pages of `insert` after the anchor page of `target`.
///
/// `after` is a 1-based page of `target` the insertion follows; `0` places
/// the pages before the first page and the page count appends them.
/// `source_pages` selects and orders the inserted pages; every page of
/// `insert` is used in order when it is `None`.
pub fn insert_pages(
    target: &Path,
    insert: &Path,
    after: usize,
    source_pages: Option<&[usize]>,
    out: &Path,
    overwrite: bool,
) -> Result<ComposeReport> {
    let total = page_count(target)?;
    let available = page_count(insert)?;
    if after > total {
        return Err(argument(format!("page {after} is outside the {total}-page document")));
    }
    let chosen = match source_pages {
        Some(pages) => {
            check_selection(pages, available)?;
            pages.to_vec()
        }
        None => (1..=available).collect(),
    };
    let mut placements = Vec::with_capacity(total + chosen.len());
    placements.extend((1..=total).take(after).map(|page| PagePlacement::Source { source: 0, page }));
    placements.extend(chosen.iter().map(|page| PagePlacement::Source { source: 1, page: *page }));
    placements.extend((1..=total).skip(after).map(|page| PagePlacement::Source { source: 0, page }));
    compose(&PagePlan { sources: vec![target.to_path_buf(), insert.to_path_buf()], placements }, out, overwrite)
}

/// Inserts `count` blank pages after the anchor page of `target`.
///
/// `after` follows [`insert_pages`]. `size` names the blank extent in points;
/// when absent the geometry of the target's first page is reused so an
/// insertion keeps the document's paper. `count` is limited to `1..=100`.
pub fn insert_blank_pages(
    target: &Path,
    after: usize,
    count: usize,
    size: Option<(f64, f64)>,
    out: &Path,
    overwrite: bool,
) -> Result<ComposeReport> {
    if !(1..=100).contains(&count) {
        return Err(argument("insert_blank_pages count must be between 1 and 100"));
    }
    let document = load_document(target)?;
    let total = document.pages.len();
    if after > total {
        return Err(argument(format!("page {after} is outside the {total}-page document")));
    }
    let (width, height) = match size {
        Some((width, height)) => {
            check_blank_size(width, height)?;
            (width, height)
        }
        None => {
            let first = document.pages.first().ok_or_else(|| argument("the target document has no pages"))?;
            (first.media_box[2], first.media_box[3])
        }
    };
    let blank = PagePlacement::Blank { width_pt: width, height_pt: height };
    let mut placements = Vec::with_capacity(total + count);
    if after == 0 {
        placements.extend(std::iter::repeat_n(blank, count));
    }
    for page in 1..=total {
        placements.push(PagePlacement::Source { source: 0, page });
        if page == after {
            placements.extend(std::iter::repeat_n(blank, count));
        }
    }
    compose(&PagePlan { sources: vec![target.to_path_buf()], placements }, out, overwrite)
}

/// Replaces the selected pages of `target` with pages of `source`.
///
/// The replacement pages take the position of the lowest selected page and
/// the unselected target pages keep their order. `source_pages` selects and
/// orders the replacement pages; every page of `source` is used in order when
/// it is `None`. Both selections must be non-empty.
pub fn replace_pages(
    target: &Path,
    selection: &[usize],
    source: &Path,
    source_pages: Option<&[usize]>,
    out: &Path,
    overwrite: bool,
) -> Result<ComposeReport> {
    if selection.is_empty() {
        return Err(argument("replace_pages needs at least one selected page"));
    }
    let total = page_count(target)?;
    check_selection(selection, total)?;
    let available = page_count(source)?;
    if available == 0 {
        return Err(argument("the replacement document has no pages"));
    }
    let chosen = match source_pages {
        Some(pages) => {
            if pages.is_empty() {
                return Err(argument("replace_pages needs at least one replacement page"));
            }
            check_selection(pages, available)?;
            pages.to_vec()
        }
        None => (1..=available).collect(),
    };
    let selected = BTreeSet::from_iter(selection.iter().copied());
    let first = *selection.iter().min().expect("a non-empty selection has a minimum");
    let mut placements = Vec::with_capacity(total + chosen.len());
    for page in 1..=total {
        if page == first {
            placements.extend(chosen.iter().map(|source| PagePlacement::Source { source: 1, page: *source }));
        }
        if !selected.contains(&page) {
            placements.push(PagePlacement::Source { source: 0, page });
        }
    }
    compose(&PagePlan { sources: vec![target.to_path_buf(), source.to_path_buf()], placements }, out, overwrite)
}

/// Alternates the pages of two documents and appends the longer remainder.
///
/// The output starts with the first page of `first`, then the first page of
/// `second`, and so on. Both documents must carry at least one page.
pub fn interleave_pages(first: &Path, second: &Path, out: &Path, overwrite: bool) -> Result<ComposeReport> {
    let first_total = page_count(first)?;
    let second_total = page_count(second)?;
    if first_total == 0 || second_total == 0 {
        return Err(argument("both documents must carry at least one page"));
    }
    let mut placements = Vec::with_capacity(first_total + second_total);
    for index in 0..first_total.max(second_total) {
        if index < first_total {
            placements.push(PagePlacement::Source { source: 0, page: index + 1 });
        }
        if index < second_total {
            placements.push(PagePlacement::Source { source: 1, page: index + 1 });
        }
    }
    compose(&PagePlan { sources: vec![first.to_path_buf(), second.to_path_buf()], placements }, out, overwrite)
}

/// Reverses the whole document or one selection in place.
///
/// When `selection` is given, those pages trade places among themselves and
/// every other page keeps its position. The selection must be non-empty and
/// repeated pages are rejected; the flip follows the given selection order.
pub fn reverse_pages(input: &Path, selection: Option<&[usize]>, out: &Path, overwrite: bool) -> Result<crate::ArrangeReport> {
    if let Some(selection) = selection
        && selection.is_empty()
    {
        return Err(argument("reverse_pages needs at least one selected page"));
    }
    let total = page_count(input)?;
    let reversed = match selection {
        Some(pages) => {
            check_selection(pages, total)?;
            pages.to_vec()
        }
        None => (1..=total).collect(),
    };
    let mut order: Vec<usize> = (1..=total).collect();
    let flipped: Vec<usize> = reversed.iter().rev().copied().collect();
    for (position, page) in reversed.iter().enumerate() {
        order[page - 1] = flipped[position];
    }
    arrange_order(input, &order, out, overwrite)
}

/// Moves the selected pages so they follow one anchor page.
///
/// `after` is a 1-based page that stays put, or `0` to move the selection to
/// the front; an anchor inside the selection is refused because its position
/// is being moved. The pages keep the relative order of the selection, so a
/// selection such as `[4, 2]` places page 4 before page 2. The selection must
/// be non-empty and repeated pages are rejected.
pub fn move_pages(input: &Path, selection: &[usize], after: usize, out: &Path, overwrite: bool) -> Result<crate::ArrangeReport> {
    if selection.is_empty() {
        return Err(argument("move_pages needs at least one selected page"));
    }
    let total = page_count(input)?;
    check_selection(selection, total)?;
    if after > total {
        return Err(argument(format!("page {after} is outside the {total}-page document")));
    }
    let selected = BTreeSet::from_iter(selection.iter().copied());
    if after != 0 && selected.contains(&after) {
        return Err(argument(format!("page {after} is part of the selection being moved")));
    }
    let mut order: Vec<usize> = (1..=total).filter(|page| !selected.contains(page)).collect();
    let position = match after {
        0 => 0,
        anchor => order.iter().position(|page| *page == anchor).expect("the anchor survives the filter") + 1,
    };
    order.splice(position..position, selection.iter().copied());
    arrange_order(input, &order, out, overwrite)
}

/// Trades places between two equal-length selections.
///
/// The first page of `left` swaps with the first of `right`, and so on. The
/// selections must be the same length, must not share a page and must not
/// repeat one; the pairing follows the given order of each selection.
pub fn swap_pages(input: &Path, left: &[usize], right: &[usize], out: &Path, overwrite: bool) -> Result<crate::ArrangeReport> {
    if left.is_empty() {
        return Err(argument("swap_pages needs at least one selected page"));
    }
    let total = page_count(input)?;
    check_selection(left, total)?;
    check_selection(right, total)?;
    if left.len() != right.len() {
        return Err(argument(format!("swap_pages selections must be the same length, got {} and {}", left.len(), right.len())));
    }
    let right_set = BTreeSet::from_iter(right.iter().copied());
    if left.iter().any(|page| right_set.contains(page)) {
        return Err(argument("swap_pages selections must not share a page"));
    }
    let mut order: Vec<usize> = (1..=total).collect();
    for (source, target) in left.iter().zip(right) {
        order.swap(source - 1, target - 1);
    }
    arrange_order(input, &order, out, overwrite)
}

/// Validates a 1-based page selection: every page exists and appears once.
fn check_selection(selection: &[usize], page_count: usize) -> Result<()> {
    let mut seen = BTreeSet::new();
    for page in selection {
        check_page_number(*page, page_count)?;
        if !seen.insert(*page) {
            return Err(argument(format!("page {page} appears twice in the selection")));
        }
    }
    Ok(())
}

fn page_count(path: &Path) -> Result<usize> {
    Ok(load_document(path)?.pages.len())
}

fn check_blank_size(width: f64, height: f64) -> Result<()> {
    if !width.is_finite()
        || !height.is_finite()
        || !(100.0..=14400.0).contains(&width)
        || !(100.0..=14400.0).contains(&height)
    {
        return Err(argument("blank page width and height must be between 100 and 14400 points"));
    }
    Ok(())
}

fn blank_page(writer: &mut PdfWriter, width: f64, height: f64) -> u32 {
    let content = writer.add(PdfObject::Stream(PdfStream { dict: Dict::new(), data: Vec::new() }));
    let mut page = Dict::new();
    page.insert("Type".to_string(), PdfObject::Name("Page".to_string()));
    page.insert("MediaBox".to_string(), media_box_object([0.0, 0.0, width, height]));
    page.insert("Contents".to_string(), PdfObject::Reference(content, 0));
    page.insert("Resources".to_string(), PdfObject::Dictionary(Dict::new()));
    writer.add(PdfObject::Dictionary(page))
}

/// Applies a full output permutation through [`arrange`] with overwrite gating.
fn arrange_order(input: &Path, order: &[usize], out: &Path, overwrite: bool) -> Result<crate::ArrangeReport> {
    if out.exists() && !overwrite {
        return Err(exists_error_at(out, format!("{} already exists", out.display())));
    }
    arrange(input, out, &DocumentArrange { order: Some(order.to_vec()), rotate: None, remove: None })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::{CreateRequest, create, extract_text, inspect};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("document-pdf-compose-{}-{sequence}", std::process::id()));
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

    /// Extracts the fixture marker of every output page, in output order.
    fn markers(path: &Path) -> Vec<String> {
        let text = extract_text(path).expect("extract output").markdown;
        text.lines().map(str::to_string).filter(|line| !line.trim().is_empty()).collect()
    }

    #[test]
    fn compose_splices_two_documents_by_plan() {
        let workspace = Workspace::new();
        let first = workspace.create("first.pdf", &["One", "Two"]);
        let second = workspace.create("second.pdf", &["Alpha", "Beta"]);
        let out = workspace.path("spliced.pdf");
        let plan = PagePlan {
            sources: vec![first.clone(), second.clone()],
            placements: vec![
                PagePlacement::Source { source: 0, page: 1 },
                PagePlacement::Source { source: 1, page: 2 },
                PagePlacement::Source { source: 0, page: 2 },
                PagePlacement::Source { source: 1, page: 1 },
            ],
        };

        let report = compose(&plan, &out, false).expect("compose");
        assert_eq!(report.pages, 4);
        assert_eq!(report.blank_pages, 0);
        assert_eq!(report.sources[0].used, 2);
        assert_eq!(report.sources[1].used, 2);
        assert_eq!(markers(&out), ["One", "Beta", "Two", "Alpha"]);
    }

    #[test]
    fn compose_with_title_overrides_the_first_source_info() {
        let workspace = Workspace::new();
        let first = workspace.path("first.pdf");
        create(&CreateRequest {
            out: first.clone(),
            title: Some("Source title".to_string()),
            pages: vec!["One".to_string()],
            overwrite: true,
            ..CreateRequest::default()
        })
        .expect("create titled fixture");
        let out = workspace.path("titled.pdf");
        let plan = PagePlan { sources: vec![first], placements: vec![PagePlacement::Source { source: 0, page: 1 }] };

        compose_with_title(&plan, &out, false, Some("Override")).expect("compose with a title");
        assert_eq!(inspect(&out).expect("inspect output").metadata.title.as_deref(), Some("Override"));
    }

    #[test]
    fn compose_without_a_title_keeps_or_omits_the_source_info() {
        let workspace = Workspace::new();
        let titled = workspace.path("titled.pdf");
        create(&CreateRequest {
            out: titled.clone(),
            title: Some("Source title".to_string()),
            pages: vec!["One".to_string()],
            overwrite: true,
            ..CreateRequest::default()
        })
        .expect("create titled fixture");
        let plain = workspace.create("plain.pdf", &["Only"]);

        let kept = workspace.path("kept.pdf");
        let plan = PagePlan { sources: vec![titled], placements: vec![PagePlacement::Source { source: 0, page: 1 }] };
        compose(&plan, &kept, false).expect("compose without a title");
        assert_eq!(inspect(&kept).expect("inspect output").metadata.title.as_deref(), Some("Source title"));

        let untitled = workspace.path("untitled.pdf");
        let plan = PagePlan { sources: vec![plain], placements: vec![PagePlacement::Source { source: 0, page: 1 }] };
        compose(&plan, &untitled, false).expect("compose an untitled source");
        assert_eq!(inspect(&untitled).expect("inspect output").metadata.title, None);
    }

    #[test]
    fn compose_repeats_pages_and_adds_blanks() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["Only"]);
        let out = workspace.path("repeated.pdf");
        let plan = PagePlan {
            sources: vec![source],
            placements: vec![
                PagePlacement::Source { source: 0, page: 1 },
                PagePlacement::Source { source: 0, page: 1 },
                PagePlacement::Blank { width_pt: 400.0, height_pt: 500.0 },
            ],
        };

        let report = compose(&plan, &out, false).expect("compose with blank");
        assert_eq!(report.pages, 3);
        assert_eq!(report.blank_pages, 1);
        let inspected = inspect(&out).expect("inspect output");
        assert_eq!(inspected.pages, 3);
        assert_eq!(inspected.page_sizes[2].width_pt, 400.0);
        assert_eq!(inspected.page_sizes[2].height_pt, 500.0);
    }

    #[test]
    fn compose_refuses_existing_output_and_bad_placements() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One"]);
        let out = workspace.path("out.pdf");

        let empty = PagePlan { sources: vec![source.clone()], placements: Vec::new() };
        assert!(compose(&empty, &out, false).is_err());

        let valid = PagePlan { sources: vec![source.clone()], placements: vec![PagePlacement::Source { source: 0, page: 1 }] };
        compose(&valid, &out, false).expect("compose once");
        assert!(compose(&valid, &out, false).unwrap_err().to_string().contains("already exists"));
        compose(&valid, &out, true).expect("compose over");

        let bad_source = PagePlan { sources: vec![source.clone()], placements: vec![PagePlacement::Source { source: 3, page: 1 }] };
        assert!(compose(&bad_source, &out, true).is_err());
        let bad_page = PagePlan { sources: vec![source], placements: vec![PagePlacement::Source { source: 0, page: 9 }] };
        assert!(compose(&bad_page, &out, true).is_err());
        let tiny = PagePlan { sources: vec![], placements: vec![PagePlacement::Blank { width_pt: 10.0, height_pt: 10.0 }] };
        assert!(compose(&tiny, &out, true).is_err());
    }

    #[test]
    fn duplicate_pages_repeats_each_selection() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three"]);
        let out = workspace.path("doubled.pdf");

        let report = duplicate_pages(&source, &[2], 3, &out, false).expect("duplicate");
        assert_eq!(report.pages, 5);
        assert_eq!(markers(&out), ["One", "Two", "Two", "Two", "Three"]);

        assert!(duplicate_pages(&source, &[2], 1, &workspace.path("low.pdf"), true).is_err());
        assert!(duplicate_pages(&source, &[], 2, &workspace.path("none.pdf"), true).is_err());
        assert!(duplicate_pages(&source, &[2, 2], 2, &workspace.path("twice.pdf"), true).is_err());
        assert!(duplicate_pages(&source, &[9], 2, &workspace.path("far.pdf"), true).is_err());
    }

    #[test]
    fn insert_pages_places_selection_after_anchor() {
        let workspace = Workspace::new();
        let target = workspace.create("target.pdf", &["One", "Two", "Three"]);
        let insert = workspace.create("insert.pdf", &["Alpha", "Beta", "Gamma"]);
        let out = workspace.path("inserted.pdf");

        let report = insert_pages(&target, &insert, 1, Some(&[3, 1]), &out, false).expect("insert middle");
        assert_eq!(report.pages, 5);
        assert_eq!(markers(&out), ["One", "Gamma", "Alpha", "Two", "Three"]);

        let front = workspace.path("front.pdf");
        insert_pages(&target, &insert, 0, None, &front, false).expect("insert front");
        assert_eq!(markers(&front), ["Alpha", "Beta", "Gamma", "One", "Two", "Three"]);

        let tail = workspace.path("tail.pdf");
        insert_pages(&target, &insert, 3, Some(&[2]), &tail, false).expect("append");
        assert_eq!(markers(&tail), ["One", "Two", "Three", "Beta"]);

        assert!(insert_pages(&target, &insert, 9, None, &workspace.path("far.pdf"), true).is_err());
        assert!(insert_pages(&target, &insert, 1, Some(&[9]), &workspace.path("bad.pdf"), true).is_err());
    }

    #[test]
    fn insert_blank_pages_reuses_or_sets_geometry() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two"]);
        let out = workspace.path("blanks.pdf");

        let report = insert_blank_pages(&source, 1, 2, None, &out, false).expect("insert blanks");
        assert_eq!(report.pages, 4);
        assert_eq!(report.blank_pages, 2);
        let inspected = inspect(&out).expect("inspect blanks");
        assert_eq!(inspected.page_sizes[1].width_pt, inspected.page_sizes[0].width_pt);
        assert_eq!(markers(&out), ["One", "Two"]);

        let sized = workspace.path("sized.pdf");
        insert_blank_pages(&source, 0, 1, Some((300.0, 700.0)), &sized, false).expect("front blank");
        let inspected = inspect(&sized).expect("inspect sized");
        assert_eq!((inspected.page_sizes[0].width_pt, inspected.page_sizes[0].height_pt), (300.0, 700.0));

        assert!(insert_blank_pages(&source, 1, 0, None, &workspace.path("zero.pdf"), true).is_err());
        assert!(insert_blank_pages(&source, 9, 1, None, &workspace.path("far.pdf"), true).is_err());
        assert!(insert_blank_pages(&source, 1, 1, Some((50.0, 50.0)), &workspace.path("tiny.pdf"), true).is_err());
    }

    #[test]
    fn replace_pages_splices_replacement_at_the_selection() {
        let workspace = Workspace::new();
        let target = workspace.create("target.pdf", &["One", "Two", "Three", "Four"]);
        let source = workspace.create("source.pdf", &["Alpha", "Beta"]);
        let out = workspace.path("replaced.pdf");

        let report = replace_pages(&target, &[2, 3], &source, None, &out, false).expect("replace");
        assert_eq!(report.pages, 4);
        assert_eq!(markers(&out), ["One", "Alpha", "Beta", "Four"]);

        let single = workspace.path("single.pdf");
        replace_pages(&target, &[4], &source, Some(&[2]), &single, false).expect("replace the tail");
        assert_eq!(markers(&single), ["One", "Two", "Three", "Beta"]);

        assert!(replace_pages(&target, &[9], &source, None, &workspace.path("far.pdf"), true).is_err());
        assert!(replace_pages(&target, &[], &source, None, &workspace.path("empty.pdf"), true).is_err());
        assert!(replace_pages(&target, &[1], &source, Some(&[]), &workspace.path("none.pdf"), true).is_err());
        assert!(replace_pages(&out, &[1], &source, None, &out, false).unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn interleave_pages_alternates_then_appends_remainder() {
        let workspace = Workspace::new();
        let first = workspace.create("first.pdf", &["One", "Two", "Three"]);
        let second = workspace.create("second.pdf", &["Alpha"]);
        let out = workspace.path("interleaved.pdf");

        let report = interleave_pages(&first, &second, &out, false).expect("interleave");
        assert_eq!(report.pages, 4);
        assert_eq!(markers(&out), ["One", "Alpha", "Two", "Three"]);

        assert!(interleave_pages(&first, &first, &out, false).unwrap_err().to_string().contains("already exists"));
        interleave_pages(&first, &second, &out, true).expect("interleave again");
    }

    #[test]
    fn reverse_pages_flips_the_document_or_a_selection() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three"]);
        let out = workspace.path("reversed.pdf");

        let report = reverse_pages(&source, None, &out, false).expect("reverse all");
        assert_eq!(report.pages, 3);
        assert_eq!(markers(&out), ["Three", "Two", "One"]);

        let middle = workspace.path("middle.pdf");
        reverse_pages(&source, Some(&[1, 2]), &middle, false).expect("reverse the first two");
        assert_eq!(markers(&middle), ["Two", "One", "Three"]);

        assert!(reverse_pages(&source, Some(&[9]), &workspace.path("far.pdf"), true).is_err());
        assert!(reverse_pages(&source, Some(&[]), &workspace.path("empty.pdf"), true).is_err());
        assert!(reverse_pages(&source, None, &out, false).unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn move_pages_relocates_a_selection_after_one_anchor() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three", "Four"]);
        let out = workspace.path("moved.pdf");

        let report = move_pages(&source, &[1], 3, &out, false).expect("move after the third");
        assert_eq!(report.pages, 4);
        assert_eq!(markers(&out), ["Two", "Three", "One", "Four"]);

        let front = workspace.path("front.pdf");
        move_pages(&source, &[3, 4], 0, &front, false).expect("move the tail to the front");
        assert_eq!(markers(&front), ["Three", "Four", "One", "Two"]);

        assert!(move_pages(&source, &[2, 3], 2, &workspace.path("anchor.pdf"), true).is_err());
        assert!(move_pages(&source, &[2], 9, &workspace.path("far.pdf"), true).is_err());
        assert!(move_pages(&source, &[], 1, &workspace.path("empty.pdf"), true).is_err());
    }

    #[test]
    fn swap_pages_trades_positions_between_selections() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three", "Four"]);
        let out = workspace.path("swapped.pdf");

        let report = swap_pages(&source, &[1, 2], &[4, 3], &out, false).expect("swap blocks");
        assert_eq!(report.pages, 4);
        assert_eq!(markers(&out), ["Four", "Three", "Two", "One"]);

        assert!(swap_pages(&source, &[1], &[1], &workspace.path("shared.pdf"), true).is_err());
        assert!(swap_pages(&source, &[1], &[3, 4], &workspace.path("uneven.pdf"), true).is_err());
        assert!(swap_pages(&source, &[1, 1], &[2, 3], &workspace.path("repeat.pdf"), true).is_err());
    }

    #[test]
    fn page_order_operations_follow_the_given_selection_order() {
        let workspace = Workspace::new();
        let source = workspace.create("source.pdf", &["One", "Two", "Three", "Four"]);
        let out = workspace.path("ordered.pdf");

        move_pages(&source, &[4, 2], 1, &out, false).expect("move in the given order");
        assert_eq!(markers(&out), ["One", "Four", "Two", "Three"]);

        let swapped = workspace.path("paired.pdf");
        swap_pages(&source, &[1, 4], &[2, 3], &swapped, false).expect("pair in the given order");
        assert_eq!(markers(&swapped), ["Two", "One", "Four", "Three"]);

        let flipped = workspace.path("flipped.pdf");
        reverse_pages(&source, Some(&[1, 2, 3]), &flipped, false).expect("flip the first three");
        assert_eq!(markers(&flipped), ["Three", "Two", "One", "Four"]);

        assert!(move_pages(&source, &[2, 2], 1, &workspace.path("twice.pdf"), true).is_err());
        assert!(reverse_pages(&source, Some(&[4, 4]), &workspace.path("again.pdf"), true).is_err());
    }

    #[test]
    fn page_order_operations_report_corrupt_documents_without_panicking() {
        let workspace = Workspace::new();
        let broken = workspace.path("broken.pdf");
        fs::write(&broken, b"not a PDF").expect("write a corrupt fixture");
        let readable = workspace.create("readable.pdf", &["One"]);

        let error = reverse_pages(&broken, None, &workspace.path("r.pdf"), true).expect_err("corrupt input");
        assert_eq!(error.code(), "format/invalid");
        let error = move_pages(&broken, &[1], 0, &workspace.path("m.pdf"), true).expect_err("corrupt input");
        assert_eq!(error.code(), "format/invalid");
        let error = swap_pages(&broken, &[1], &[2], &workspace.path("s.pdf"), true).expect_err("corrupt input");
        assert_eq!(error.code(), "format/invalid");
        let error = interleave_pages(&broken, &readable, &workspace.path("i.pdf"), true).expect_err("corrupt input");
        assert_eq!(error.code(), "format/invalid");
    }
}
