//! Named destinations and link annotations on top of the shared engine.
//!
//! Page order operations move pages; this module answers where a document's
//! links and named destinations point. [`list_destinations`] reads the
//! `/Root/Dests` name tree, and [`extract_links`] walks every `/Link`
//! annotation and resolves its target: a URI, a destination inside this
//! document (direct reference, destination array or named destination) or a
//! destination inside an embedded document carried by a `GoToE` action.
//! [`list_outline`] walks the `/Root/Outlines` tree and reports each entry with
//! the page its destination resolves to, reusing the same resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::object::{Dict, PdfObject};
use crate::parse::{PdfDocument, decode_text_string, parse_document, resolve_object};
use crate::{filters, load_document};

/// The deepest name tree walked before giving up, guarding against cycles.
const MAX_NAME_TREE_DEPTH: usize = 32;

/// A named destination resolved to a page of its document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedDestination {
    /// Destination name as spelled in the name tree.
    pub name: String,
    /// 1-based page the destination points at.
    pub page: usize,
}

/// One outline entry with the page its destination resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineEntry {
    /// Outline title as spelled in the outline tree.
    pub title: String,
    /// Nesting level, top-level entries are `1`.
    pub depth: usize,
    /// 1-based page the entry points at, or `null` when the destination is
    /// missing, names an unknown destination or leaves this document.
    pub page: Option<usize>,
}

/// The action kind carried by a link annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkKind {
    /// Opens an external URI.
    Uri,
    /// Jumps to a destination inside this document.
    GoTo,
    /// Jumps to a destination inside an embedded document.
    GoToEmbedded,
    /// Any other action, or an annotation that only carries a raw destination.
    Other,
}

/// One link annotation as reported by [`extract_links`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRecord {
    /// 1-based page carrying the annotation.
    pub page: usize,
    /// Annotation rectangle when the dictionary carries four numbers.
    pub rect: Option<[f64; 4]>,
    /// The action kind.
    pub kind: LinkKind,
    /// External URI, or the destination name carried by the action.
    pub target: Option<String>,
    /// 1-based page the link resolves to: in this document for [`LinkKind::GoTo`],
    /// in the embedded document for [`LinkKind::GoToEmbedded`].
    pub destination: Option<usize>,
}

/// Lists the named destinations in a PDF's name trees.
///
/// `/Root/Dests` is read first and `/Root/Names/Dests` is used as the
/// classic fallback. Destinations that do not resolve to a page of the
/// document are skipped.
pub fn list_destinations(path: &Path) -> Result<Vec<NamedDestination>> {
    let document = load_document(path)?;
    let dests = destination_map(&document);
    let mut out = Vec::new();
    for (name, value) in &dests {
        if let Some(page) = resolve_destination(&document, &dests, value) {
            out.push(NamedDestination { name: name.clone(), page });
        }
    }
    Ok(out)
}

/// Lists every outline entry with the page its destination resolves to.
///
/// Entries keep outline reading order: an entry is followed by its children and
/// then by its next sibling. `/Dest` and a `GoTo` action `/D` are both accepted;
/// an entry whose destination cannot be resolved to a page of this document
/// keeps `page: None`, so a product can reject it instead of guessing.
pub fn list_outline(path: &Path) -> Result<Vec<OutlineEntry>> {
    let document = load_document(path)?;
    Ok(outline_entries(&document))
}

/// Collects the outline tree of one parsed document.
fn outline_entries(document: &PdfDocument) -> Vec<OutlineEntry> {
    let Some(root) = document
        .catalog
        .get("Outlines")
        .and_then(|node| resolve_object(&document.objects, node).ok())
        .and_then(PdfObject::as_dict)
    else {
        return Vec::new();
    };
    let dests = destination_map(document);
    let mut entries = Vec::new();
    let mut visited = BTreeSet::new();
    if let Some(first) = root.get("First") {
        walk_outline(document, &dests, first, 1, &mut visited, &mut entries);
    }
    entries
}

/// Walks one sibling chain, descending into children first.
fn walk_outline(
    document: &PdfDocument,
    dests: &BTreeMap<String, PdfObject>,
    node: &PdfObject,
    depth: usize,
    visited: &mut BTreeSet<u32>,
    entries: &mut Vec<OutlineEntry>,
) {
    if depth > MAX_NAME_TREE_DEPTH {
        return;
    }
    let mut current = Some(node.clone());
    while let Some(object) = current {
        if let PdfObject::Reference(number, _) = object
            && !visited.insert(number)
        {
            return;
        }
        let Some(item) = resolve_object(&document.objects, &object).ok().and_then(PdfObject::as_dict).cloned() else {
            return;
        };
        let title = item
            .get("Title")
            .and_then(|title| match title {
                PdfObject::String(text) => Some(decode_text_string(text)),
                _ => None,
            })
            .unwrap_or_default();
        entries.push(OutlineEntry { title, depth, page: outline_page(document, dests, &item) });
        if let Some(first) = item.get("First") {
            walk_outline(document, dests, first, depth + 1, visited, entries);
        }
        current = item.get("Next").cloned();
    }
}

/// Resolves the page of one outline item: its `/Dest` or a `GoTo` action `/D`.
fn outline_page(document: &PdfDocument, dests: &BTreeMap<String, PdfObject>, item: &Dict) -> Option<usize> {
    let destination = match item.get("Dest") {
        Some(value) => value,
        None => {
            let action = item.get("A").and_then(|value| resolve_object(&document.objects, value).ok())?;
            let action = action.as_dict()?;
            if action.get("S").and_then(PdfObject::as_name) != Some("GoTo") {
                return None;
            }
            action.get("D")?
        }
    };
    resolve_destination(document, dests, destination)
}

/// Reads every `/Link` annotation and resolves its target.
///
/// Links are reported in page order and annotation order. A `GoToE` target is
/// resolved inside the embedded PDF when the action carries both a file and a
/// destination; the embedded document name is reported as `target`.
/// Unresolvable targets leave `destination` null.
pub fn extract_links(path: &Path) -> Result<Vec<LinkRecord>> {
    let document = load_document(path)?;
    let dests = destination_map(&document);
    let mut links = Vec::new();
    for (index, page) in document.pages.iter().enumerate() {
        let Some(annots) = page
            .own
            .get("Annots")
            .and_then(|annots| resolve_object(&document.objects, annots).ok())
            .and_then(|annots| annots.as_array().map(|values| values.to_vec()))
        else {
            continue;
        };
        for annotation in &annots {
            let Some(annotation) = resolve_object(&document.objects, annotation).ok().and_then(|object| object.as_dict().cloned()) else {
                continue;
            };
            if annotation.get("Subtype").and_then(|subtype| subtype.as_name()) != Some("Link") {
                continue;
            }
            let rect = annotation
                .get("Rect")
                .and_then(|rect| resolve_object(&document.objects, rect).ok())
                .and_then(|rect| rect.as_array().map(|values| values.to_vec()))
                .filter(|values| values.len() == 4)
                .and_then(|values| values.iter().filter_map(number).collect::<Vec<f64>>().try_into().ok());
            let mut kind = LinkKind::Other;
            let mut target = None;
            let mut destination = None;
            let action = annotation
                .get("A")
                .and_then(|action| resolve_object(&document.objects, action).ok())
                .and_then(|action| action.as_dict().cloned());
            if let Some(action) = action {
                match action.get("S").and_then(|name| name.as_name()) {
                    Some("URI") => {
                        kind = LinkKind::Uri;
                        target = action.get("URI").and_then(|uri| match uri {
                            PdfObject::String(bytes) => Some(decode_text_string(bytes)),
                            _ => None,
                        });
                    }
                    Some("GoTo") => {
                        if let Some(value) = action.get("D") {
                            resolve_goto(&document, &dests, value, &mut kind, &mut target, &mut destination);
                        }
                    }
                    Some("GoToE") => {
                        kind = LinkKind::GoToEmbedded;
                        let (name, page) = embedded_target(&document, action.get("F"), action.get("D"));
                        target = name;
                        destination = page;
                    }
                    _ => {}
                }
            }
            if kind == LinkKind::Other
                && let Some(value) = annotation.get("Dest")
            {
                resolve_goto(&document, &dests, value, &mut kind, &mut target, &mut destination);
            }
            links.push(LinkRecord { page: index + 1, rect, kind, target, destination });
        }
    }
    Ok(links)
}

/// Resolves a `GoTo` destination value: a name through the destination map,
/// an array through its first entry, anything else as a direct page object.
fn resolve_goto(
    document: &PdfDocument,
    dests: &BTreeMap<String, PdfObject>,
    value: &PdfObject,
    kind: &mut LinkKind,
    target: &mut Option<String>,
    destination: &mut Option<usize>,
) {
    *kind = LinkKind::GoTo;
    match value {
        PdfObject::String(text) => {
            let name = decode_text_string(text);
            *target = Some(name.clone());
            *destination = dests.get(&name).and_then(|value| resolve_destination(document, dests, value));
        }
        PdfObject::Name(name) => {
            *target = Some(name.clone());
            *destination = dests.get(name).and_then(|value| resolve_destination(document, dests, value));
        }
        other => *destination = resolve_destination(document, dests, other),
    }
}

/// Resolves a `GoToE` action into the embedded document name and the page its
/// destination points at inside that embedded PDF.
fn embedded_target(document: &PdfDocument, file: Option<&PdfObject>, destination: Option<&PdfObject>) -> (Option<String>, Option<usize>) {
    let name = file.and_then(|spec| embedded_file_name(document, spec));
    let (Some(file), Some(destination)) = (file, destination) else {
        return (name, None);
    };
    let stream = embedded_stream(document, file);
    let Some(stream) = stream else {
        return (name, None);
    };
    let Ok(bytes) = filters::decode(&stream, &document.objects) else {
        return (name, None);
    };
    let Ok(embedded) = parse_document(&bytes) else {
        return (name, None);
    };
    let embedded_dests = destination_map(&embedded);
    (name, resolve_destination(&embedded, &embedded_dests, destination))
}

/// The decoded stream carried by a `GoToE` file specification.
fn embedded_stream(document: &PdfDocument, file: &PdfObject) -> Option<crate::object::PdfStream> {
    let spec = resolve_object(&document.objects, file).ok()?.as_dict()?.clone();
    let ef = spec.get("EF").and_then(|ef| resolve_object(&document.objects, ef).ok())?.as_dict()?.clone();
    let entry = ef.get("F").or_else(|| ef.get("UF"))?.clone();
    match resolve_object(&document.objects, &entry).ok()? {
        PdfObject::Stream(stream) => Some(stream.clone()),
        _ => None,
    }
}

/// The user-visible name of the file specification, preferring `/UF`.
fn embedded_file_name(document: &PdfDocument, spec: &PdfObject) -> Option<String> {
    let spec = resolve_object(&document.objects, spec).ok()?.as_dict()?.clone();
    for key in ["UF", "DOS", "F"] {
        if let Some(name) = spec.get(key).and_then(|value| match value {
            PdfObject::String(bytes) => Some(decode_text_string(bytes)),
            _ => None,
        }) {
            return Some(name);
        }
    }
    None
}

/// Collects the named destination map from `/Root/Dests` and the classic
/// `/Root/Names/Dests` fallback.
fn destination_map(document: &PdfDocument) -> BTreeMap<String, PdfObject> {
    let mut dests = BTreeMap::new();
    if let Some(node) = document.catalog.get("Dests") {
        collect_name_tree(&document.objects, node, &mut dests, 0);
    }
    if dests.is_empty()
        && let Some(node) = document.catalog.get("Names").and_then(|names| names.as_dict()).and_then(|names| names.get("Dests"))
    {
        collect_name_tree(&document.objects, node, &mut dests, 0);
    }
    dests
}

/// Walks a name tree, reading `/Names` pairs and recursing through `/Kids`.
fn collect_name_tree(objects: &BTreeMap<u32, PdfObject>, node: &PdfObject, out: &mut BTreeMap<String, PdfObject>, depth: usize) {
    if depth > MAX_NAME_TREE_DEPTH {
        return;
    }
    let Ok(PdfObject::Dictionary(dict)) = resolve_object(objects, node) else {
        return;
    };
    if let Some(items) = dict.get("Names").and_then(|names| names.as_array()) {
        for pair in items.chunks(2) {
            if let (Some(PdfObject::String(bytes)), Some(value)) = (pair.first(), pair.get(1)) {
                out.insert(decode_text_string(bytes), value.clone());
            }
        }
    }
    if let Some(kids) = dict.get("Kids").and_then(|kids| kids.as_array()) {
        for kid in kids {
            collect_name_tree(objects, kid, out, depth + 1);
        }
    }
}

/// Resolves a destination value to a 1-based page of `document`.
fn resolve_destination(document: &PdfDocument, dests: &BTreeMap<String, PdfObject>, destination: &PdfObject) -> Option<usize> {
    let page_object = match destination {
        PdfObject::Array(values) => match values.first() {
            Some(first) => first,
            None => return None,
        },
        PdfObject::String(bytes) => {
            let name = decode_text_string(bytes);
            let value = dests.get(&name)?;
            return resolve_destination(document, dests, value);
        }
        PdfObject::Name(name) => {
            let value = dests.get(name)?;
            return resolve_destination(document, dests, value);
        }
        other => other,
    };
    // Destinations carry the page's object number; matching by number keeps
    // identical page dictionaries (two blank pages, say) distinguishable.
    if let PdfObject::Reference(number, _) = page_object
        && let Some(index) = page_tree_numbers(document).and_then(|numbers| numbers.iter().position(|slot| *slot == *number))
    {
        return Some(index + 1);
    }
    let dict = match resolve_object(&document.objects, page_object).ok()? {
        PdfObject::Dictionary(dict) => dict.clone(),
        _ => return None,
    };
    document.pages.iter().position(|page| page.own == dict).map(|index| index + 1)
}

/// The page object numbers in reading order, walked from `/Catalog/Pages`.
fn page_tree_numbers(document: &PdfDocument) -> Option<Vec<u32>> {
    let mut numbers = Vec::new();
    collect_page_numbers(&document.objects, document.catalog.get("Pages")?, &mut numbers, 0)?;
    Some(numbers)
}

fn collect_page_numbers(objects: &BTreeMap<u32, PdfObject>, node: &PdfObject, out: &mut Vec<u32>, depth: usize) -> Option<()> {
    if depth > MAX_NAME_TREE_DEPTH {
        return None;
    }
    let dict = resolve_object(objects, node).ok()?.as_dict()?;
    if dict.get("Type").and_then(PdfObject::as_name) == Some("Pages") {
        for kid in dict.get("Kids").and_then(PdfObject::as_array)? {
            collect_page_numbers(objects, kid, out, depth + 1)?;
        }
    } else if let PdfObject::Reference(number, _) = node {
        out.push(*number);
    }
    Some(())
}

fn number(object: &PdfObject) -> Option<f64> {
    match object {
        PdfObject::Integer(value) => Some(*value as f64),
        PdfObject::Real(value) => Some(*value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::object::{Dict, PdfStream};
    use crate::writer::{PdfWriter, media_box_object};
    use crate::{CreateRequest, create};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("document-pdf-links-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&root).expect("create test workspace");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
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

    fn string(value: &str) -> PdfObject {
        PdfObject::String(value.as_bytes().to_vec())
    }

    fn name(value: &str) -> PdfObject {
        PdfObject::Name(value.to_string())
    }

    /// Builds a two-page document whose first page carries a named destination.
    fn embedded_bytes() -> Vec<u8> {
        let mut writer = PdfWriter::new();
        let mut pages = Vec::new();
        for _ in 0..2 {
            let mut page = Dict::new();
            page.insert("Type".to_string(), name("Page"));
            page.insert("MediaBox".to_string(), media_box_object([0.0, 0.0, 612.0, 792.0]));
            pages.push(writer.add(PdfObject::Dictionary(page)));
        }
        let mut dest = Dict::new();
        dest.insert("Names".to_string(), PdfObject::Array(vec![string("Inner"), PdfObject::Array(vec![PdfObject::Reference(pages[1], 0), name("XYZ"), PdfObject::Null, PdfObject::Null, PdfObject::Null])]));
        let mut catalog_entries = Dict::new();
        catalog_entries.insert("Dests".to_string(), PdfObject::Dictionary(dest));
        crate::finish_document_with(&mut writer, &pages, None, catalog_entries).expect("finish embedded fixture")
    }

    /// Builds a fixture with one URI link, a named `GoTo`, a raw `Dest`,
    /// and a `GoToE` into the embedded document above.
    fn links_fixture(path: &Path) {
        let mut writer = PdfWriter::new();
        let embedded = writer.add(PdfObject::Stream(PdfStream { dict: {
            let mut dict = Dict::new();
            dict.insert("Type".to_string(), name("EmbeddedFile"));
            dict
        }, data: embedded_bytes() }));
        let mut filespec = Dict::new();
        filespec.insert("Type".to_string(), name("Filespec"));
        filespec.insert("UF".to_string(), string("inner.pdf"));
        let mut ef = Dict::new();
        ef.insert("F".to_string(), PdfObject::Reference(embedded, 0));
        filespec.insert("EF".to_string(), PdfObject::Dictionary(ef));
        let filespec = writer.add(PdfObject::Dictionary(filespec));

        let mut page_one = Dict::new();
        page_one.insert("Type".to_string(), name("Page"));
        page_one.insert("MediaBox".to_string(), media_box_object([0.0, 0.0, 612.0, 792.0]));
        let mut page_two = Dict::new();
        page_two.insert("Type".to_string(), name("Page"));
        page_two.insert("MediaBox".to_string(), media_box_object([0.0, 0.0, 612.0, 792.0]));
        let pages = vec![writer.add(PdfObject::Dictionary(page_one)), writer.add(PdfObject::Dictionary(page_two))];

        let mut uri_action = Dict::new();
        uri_action.insert("S".to_string(), name("URI"));
        uri_action.insert("URI".to_string(), string("https://example.com/report"));
        let mut uri_annot = Dict::new();
        uri_annot.insert("Subtype".to_string(), name("Link"));
        uri_annot.insert("Rect".to_string(), PdfObject::Array(vec![PdfObject::Integer(10), PdfObject::Integer(10), PdfObject::Integer(100), PdfObject::Integer(30)]));
        uri_annot.insert("A".to_string(), PdfObject::Dictionary(uri_action));

        let mut named_action = Dict::new();
        named_action.insert("S".to_string(), name("GoTo"));
        named_action.insert("D".to_string(), string("Target"));
        let mut named_annot = Dict::new();
        named_annot.insert("Subtype".to_string(), name("Link"));
        named_annot.insert("A".to_string(), PdfObject::Dictionary(named_action));

        let mut raw_dest = Dict::new();
        raw_dest.insert("Subtype".to_string(), name("Link"));
        raw_dest.insert("Dest".to_string(), PdfObject::Array(vec![PdfObject::Reference(pages[0], 0), name("Fit")]));

        let mut embedded_action = Dict::new();
        embedded_action.insert("S".to_string(), name("GoToE"));
        embedded_action.insert("F".to_string(), PdfObject::Reference(filespec, 0));
        embedded_action.insert("D".to_string(), string("Inner"));
        let mut embedded_annot = Dict::new();
        embedded_annot.insert("Subtype".to_string(), name("Link"));
        embedded_annot.insert("A".to_string(), PdfObject::Dictionary(embedded_action));

        let annots_page_one: Vec<u32> = vec![writer.add(PdfObject::Dictionary(uri_annot)), writer.add(PdfObject::Dictionary(named_annot))];
        let annots_page_two: Vec<u32> = vec![writer.add(PdfObject::Dictionary(raw_dest)), writer.add(PdfObject::Dictionary(embedded_annot))];

        for (number, annots) in pages.iter().zip([annots_page_one, annots_page_two]) {
            if let Some(PdfObject::Dictionary(page)) = writer.get_mut(*number) {
                page.insert("Annots".to_string(), PdfObject::Array(annots.into_iter().map(|number| PdfObject::Reference(number, 0)).collect()));
            }
        }

        let mut dests = Dict::new();
        dests.insert("Names".to_string(), PdfObject::Array(vec![
            string("Target"),
            PdfObject::Array(vec![PdfObject::Reference(pages[1], 0), name("XYZ"), PdfObject::Null, PdfObject::Null, PdfObject::Null]),
        ]));
        let mut catalog_entries = Dict::new();
        catalog_entries.insert("Dests".to_string(), PdfObject::Dictionary(dests));
        let bytes = crate::finish_document_with(&mut writer, &pages, None, catalog_entries).expect("finish links fixture");
        fs::write(path, bytes).expect("write links fixture");
    }

    /// Builds a three-page document whose outline mixes a named target, a
    /// direct page object, an unknown name and a dangling page reference.
    fn outline_fixture(path: &Path) {
        let mut writer = PdfWriter::new();
        let mut pages = Vec::new();
        for _ in 0..3 {
            let mut page = Dict::new();
            page.insert("Type".to_string(), name("Page"));
            page.insert("MediaBox".to_string(), media_box_object([0.0, 0.0, 612.0, 792.0]));
            pages.push(writer.add(PdfObject::Dictionary(page)));
        }
        let root = writer.reserve();
        let named = writer.reserve();
        let child = writer.reserve();
        let unknown = writer.reserve();
        let dangling = writer.reserve();

        let mut root_dict = Dict::new();
        root_dict.insert("Type".to_string(), name("Outlines"));
        root_dict.insert("First".to_string(), PdfObject::Reference(named, 0));
        root_dict.insert("Last".to_string(), PdfObject::Reference(dangling, 0));
        root_dict.insert("Count".to_string(), PdfObject::Integer(4));
        writer.set(root, PdfObject::Dictionary(root_dict));

        let mut named_dict = Dict::new();
        named_dict.insert("Title".to_string(), string("Named"));
        named_dict.insert("Parent".to_string(), PdfObject::Reference(root, 0));
        named_dict.insert("Dest".to_string(), string("Target"));
        named_dict.insert("First".to_string(), PdfObject::Reference(child, 0));
        named_dict.insert("Last".to_string(), PdfObject::Reference(child, 0));
        named_dict.insert("Count".to_string(), PdfObject::Integer(1));
        named_dict.insert("Next".to_string(), PdfObject::Reference(unknown, 0));
        writer.set(named, PdfObject::Dictionary(named_dict));

        let mut child_dict = Dict::new();
        child_dict.insert("Title".to_string(), string("Child"));
        child_dict.insert("Parent".to_string(), PdfObject::Reference(named, 0));
        child_dict.insert("Dest".to_string(), PdfObject::Array(vec![PdfObject::Reference(pages[2], 0), name("Fit")]));
        writer.set(child, PdfObject::Dictionary(child_dict));

        let mut unknown_action = Dict::new();
        unknown_action.insert("S".to_string(), name("GoTo"));
        unknown_action.insert("D".to_string(), string("Missing"));
        let mut unknown_dict = Dict::new();
        unknown_dict.insert("Title".to_string(), string("Broken"));
        unknown_dict.insert("Parent".to_string(), PdfObject::Reference(root, 0));
        unknown_dict.insert("Prev".to_string(), PdfObject::Reference(named, 0));
        unknown_dict.insert("Next".to_string(), PdfObject::Reference(dangling, 0));
        unknown_dict.insert("A".to_string(), PdfObject::Dictionary(unknown_action));
        writer.set(unknown, PdfObject::Dictionary(unknown_dict));

        let mut dangling_dict = Dict::new();
        dangling_dict.insert("Title".to_string(), string("OutOfRange"));
        dangling_dict.insert("Parent".to_string(), PdfObject::Reference(root, 0));
        dangling_dict.insert("Prev".to_string(), PdfObject::Reference(unknown, 0));
        dangling_dict.insert("Dest".to_string(), PdfObject::Array(vec![PdfObject::Reference(99, 0), name("Fit")]));
        writer.set(dangling, PdfObject::Dictionary(dangling_dict));

        let mut dests = Dict::new();
        dests.insert("Names".to_string(), PdfObject::Array(vec![
            string("Target"),
            PdfObject::Array(vec![PdfObject::Reference(pages[1], 0), name("Fit")]),
        ]));
        let mut catalog_entries = Dict::new();
        catalog_entries.insert("Outlines".to_string(), PdfObject::Reference(root, 0));
        catalog_entries.insert("Dests".to_string(), PdfObject::Dictionary(dests));
        let bytes = crate::finish_document_with(&mut writer, &pages, None, catalog_entries).expect("finish outline fixture");
        fs::write(path, bytes).expect("write outline fixture");
    }

    #[test]
    fn list_outline_reports_named_direct_broken_and_out_of_range_targets() {
        let workspace = Workspace::new();
        let path = workspace.path("outline.pdf");
        outline_fixture(&path);

        let entries = list_outline(&path).expect("list outline");
        assert_eq!(
            entries,
            [
                OutlineEntry { title: "Named".to_string(), depth: 1, page: Some(2) },
                OutlineEntry { title: "Child".to_string(), depth: 2, page: Some(3) },
                OutlineEntry { title: "Broken".to_string(), depth: 1, page: None },
                OutlineEntry { title: "OutOfRange".to_string(), depth: 1, page: None },
            ]
        );
    }

    #[test]
    fn extract_links_resolves_named_direct_and_embedded_targets() {
        let workspace = Workspace::new();
        let path = workspace.path("links.pdf");
        links_fixture(&path);

        let links = extract_links(&path).expect("extract links");
        assert_eq!(links.len(), 4);
        assert_eq!(
            links[0],
            LinkRecord {
                page: 1,
                rect: Some([10.0, 10.0, 100.0, 30.0]),
                kind: LinkKind::Uri,
                target: Some("https://example.com/report".to_string()),
                destination: None,
            }
        );
        assert_eq!(
            links[1],
            LinkRecord {
                page: 1,
                rect: None,
                kind: LinkKind::GoTo,
                target: Some("Target".to_string()),
                destination: Some(2),
            }
        );
        assert_eq!(
            links[2],
            LinkRecord {
                page: 2,
                rect: None,
                kind: LinkKind::GoTo,
                target: None,
                destination: Some(1),
            }
        );
        assert_eq!(
            links[3],
            LinkRecord {
                page: 2,
                rect: None,
                kind: LinkKind::GoToEmbedded,
                target: Some("inner.pdf".to_string()),
                destination: Some(2),
            }
        );
    }

    #[test]
    fn list_destinations_resolves_the_name_tree() {
        let workspace = Workspace::new();
        let path = workspace.path("links.pdf");
        links_fixture(&path);

        let destinations = list_destinations(&path).expect("list destinations");
        assert_eq!(destinations, [NamedDestination { name: "Target".to_string(), page: 2 }]);
    }

    #[test]
    fn documents_without_links_report_nothing() {
        let workspace = Workspace::new();
        let path = workspace.create("plain.pdf", &["One", "Two"]);
        assert!(extract_links(&path).expect("extract links").is_empty());
        assert!(list_destinations(&path).expect("list destinations").is_empty());
        assert!(list_outline(&path).expect("list outline").is_empty());
    }
}
