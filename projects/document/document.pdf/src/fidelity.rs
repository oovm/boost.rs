//! Structured fidelity reporting for the page operations.
//!
//! Every operation that rebuilds a document through the shared writer keeps
//! only the page tree and the information dictionary: catalog entries such as
//! `/Outlines`, `/AcroForm` and named destinations, and the annotations of
//! pages a plan does not place, are left out of the output. Rather than
//! dropping them silently, [`crate::compose`], [`crate::arrange`],
//! [`crate::merge`], [`crate::split`], [`crate::delete_pages`],
//! [`crate::rotate_pages`] and [`crate::set_page_boxes`] report each omission as
//! a [`FidelityLoss`] with a stable reason code, so a product can tell callers
//! exactly what a complex or damaged source lost in translation.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::object::PdfObject;
use crate::parse::{PdfDocument, resolve_object};

/// Stable reason code for a [`FidelityLoss`].
///
/// Codes serialize as kebab-case strings and are part of the public contract;
/// callers match on them instead of parsing the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FidelityCode {
    /// The source carried `/Outlines`; the rebuilt catalog has no outline tree.
    OutlinesDropped,
    /// The source carried `/AcroForm`; field definitions are not rewritten.
    AcroFormDropped,
    /// The source carried named destinations (`/Dests` or `/Names`).
    DestinationsDropped,
    /// The source carried another catalog entry the writer does not keep.
    CatalogEntryDropped,
    /// A page the plan did not place carried `/Annots`; they left with it.
    AnnotationsDropped,
}

/// One object a page operation dropped from the output.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FidelityLoss {
    /// Stable reason code.
    pub code: FidelityCode,
    /// 0-based index of the source document the loss belongs to.
    pub source: usize,
    /// 1-based page for page-scoped losses, `null` for document-scoped ones.
    pub page: Option<usize>,
    /// The dropped PDF entry, such as `/Outlines` or `/Annots`.
    pub entry: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Reports what rebuilding one source into a new page tree loses.
///
/// `kept` names the 1-based pages the plan places into the output; a page
/// outside that set leaves its `/Annots` behind. The catalog is rebuilt from
/// scratch, so every entry except `/Type` and `/Pages` is reported. Catalog
/// losses come first in dictionary order, then page losses in page order.
pub(crate) fn losses(source: usize, document: &PdfDocument, kept: &BTreeSet<usize>) -> Vec<FidelityLoss> {
    let mut losses = Vec::new();
    for key in document.catalog.keys() {
        if key == "Type" || key == "Pages" {
            continue;
        }
        let code = match key.as_str() {
            "Outlines" => FidelityCode::OutlinesDropped,
            "AcroForm" => FidelityCode::AcroFormDropped,
            "Dests" | "Names" => FidelityCode::DestinationsDropped,
            _ => FidelityCode::CatalogEntryDropped,
        };
        losses.push(FidelityLoss {
            code,
            source,
            page: None,
            entry: format!("/{key}"),
            message: format!("the rebuilt catalog does not carry /{key}"),
        });
    }
    for (index, page) in document.pages.iter().enumerate() {
        let number = index + 1;
        if kept.contains(&number) {
            continue;
        }
        let Some(annots) = page.own.get("Annots") else {
            continue;
        };
        let count = match resolve_object(&document.objects, annots) {
            Ok(PdfObject::Array(items)) => Some(items.len()),
            _ => None,
        };
        losses.push(FidelityLoss {
            code: FidelityCode::AnnotationsDropped,
            source,
            page: Some(number),
            entry: "/Annots".to_string(),
            message: match count {
                Some(count) => format!("page {number} carried {count} annotations that left with the page"),
                None => format!("page {number} carried annotations that left with the page"),
            },
        });
    }
    losses
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::{
        CreateRequest, DocumentArrange, PagePlacement, PagePlan, arrange, compose, create, delete_pages, merge, rotate_pages,
        set_page_boxes, split, split_every,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("document-pdf-fidelity-{}-{sequence}", std::process::id()));
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
        let mut output = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(output.len());
            output.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        let xref = output.len();
        output.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        output.extend_from_slice(
            format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
        );
        output
    }

    /// A two-page document with outlines, a form, named destinations, a foreign
    /// catalog entry and one annotation carried only by the first page.
    fn complex_pdf() -> Vec<u8> {
        assemble(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R /AcroForm 6 0 R /Dests 10 0 R /Lang (en) >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Annots [7 0 R] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] >>",
            "<< /Type /Outlines /First 8 0 R /Last 8 0 R /Count 1 >>",
            "<< /Fields [9 0 R] >>",
            "<< /Type /Annot /Subtype /Link /Rect [10 10 50 30] /Dest [4 0 R /Fit] >>",
            "<< /Title (Start) /Dest [3 0 R /Fit] /Parent 5 0 R >>",
            "<< /T (name) /FT /Tx /V (x) /P 3 0 R /Rect [20 20 120 40] >>",
            "<< /Names [(target) [4 0 R /Fit]] >>",
        ])
    }

    /// The stable reason codes of a report, as serialized.
    fn codes(losses: &[FidelityLoss]) -> Vec<String> {
        let value = serde_json::to_vec(losses).expect("serialize losses");
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&value).expect("losses serialize to an array");
        parsed.iter().map(|loss| loss["code"].as_str().expect("every loss carries a code").to_string()).collect()
    }

    #[test]
    fn compose_reports_catalog_entries_and_unplaced_annotations() {
        let workspace = Workspace::new();
        let source = workspace.write("complex.pdf", &complex_pdf());
        let out = workspace.path("kept-second.pdf");
        let plan = PagePlan { sources: vec![source], placements: vec![PagePlacement::Source { source: 0, page: 2 }] };

        let report = compose(&plan, &out, false).expect("compose");
        assert_eq!(
            codes(&report.fidelity),
            [
                "acro-form-dropped",
                "destinations-dropped",
                "catalog-entry-dropped",
                "outlines-dropped",
                "annotations-dropped",
            ]
        );
        let lang = report.fidelity.iter().find(|loss| loss.entry == "/Lang").expect("the foreign entry is named");
        assert_eq!(lang.source, 0);
        assert_eq!(lang.page, None);
        let annots = report.fidelity.iter().find(|loss| loss.code == FidelityCode::AnnotationsDropped).expect("page loss");
        assert_eq!((annots.page, annots.entry.as_str()), (Some(1), "/Annots"));
        assert!(annots.message.contains("1 annotation"), "{}", annots.message);
    }

    #[test]
    fn compose_keeps_placed_page_annotations_out_of_the_report() {
        let workspace = Workspace::new();
        let source = workspace.write("complex.pdf", &complex_pdf());
        let out = workspace.path("both.pdf");
        let plan = PagePlan {
            sources: vec![source],
            placements: vec![PagePlacement::Source { source: 0, page: 1 }, PagePlacement::Source { source: 0, page: 2 }],
        };

        let report = compose(&plan, &out, false).expect("compose");
        assert!(!codes(&report.fidelity).contains(&"annotations-dropped".to_string()));
        assert_eq!(codes(&report.fidelity).len(), 4, "only the catalog entries are dropped: {:?}", report.fidelity);
    }

    #[test]
    fn plain_documents_report_no_losses() {
        let workspace = Workspace::new();
        let source = workspace.create("plain.pdf", &["One", "Two"]);
        let out = workspace.path("plain-out.pdf");
        let plan = PagePlan {
            sources: vec![source.clone()],
            placements: vec![PagePlacement::Source { source: 0, page: 1 }, PagePlacement::Source { source: 0, page: 2 }],
        };

        let report = compose(&plan, &out, false).expect("compose");
        assert!(report.fidelity.is_empty(), "{:?}", report.fidelity);

        let arranged = arrange(&source, &workspace.path("plain-arrange.pdf"), &DocumentArrange::default()).expect("arrange");
        assert!(arranged.fidelity.is_empty(), "{:?}", arranged.fidelity);
    }

    #[test]
    fn arrange_reports_losses_only_for_pages_the_plan_removes() {
        let workspace = Workspace::new();
        let source = workspace.write("complex.pdf", &complex_pdf());

        let kept = arrange(&source, &workspace.path("order.pdf"), &DocumentArrange {
            order: Some(vec![2, 1]),
            ..DocumentArrange::default()
        })
        .expect("arrange");
        assert!(!codes(&kept.fidelity).contains(&"annotations-dropped".to_string()));

        let thinned = arrange(&source, &workspace.path("remove.pdf"), &DocumentArrange {
            remove: Some(vec![1]),
            ..DocumentArrange::default()
        })
        .expect("arrange remove");
        let annotation = thinned.fidelity.iter().find(|loss| loss.code == FidelityCode::AnnotationsDropped).expect("page loss");
        assert_eq!(annotation.page, Some(1));
    }

    #[test]
    fn merge_delete_rotate_and_crop_report_fidelity() {
        let workspace = Workspace::new();
        let source = workspace.write("complex.pdf", &complex_pdf());
        let plain = workspace.create("plain.pdf", &["Only"]);

        let merged = merge(&[source.clone(), plain], &workspace.path("merged.pdf"), false, None).expect("merge");
        assert_eq!(codes(&merged.fidelity).iter().filter(|code| *code == "outlines-dropped").count(), 1);
        assert!(!codes(&merged.fidelity).contains(&"annotations-dropped".to_string()));

        let deleted = delete_pages(&source, "1", &workspace.path("deleted.pdf"), false).expect("delete");
        assert_eq!(deleted.fidelity.iter().filter(|loss| loss.code == FidelityCode::AnnotationsDropped).count(), 1);

        let rotated = rotate_pages(&source, "2", 90, &workspace.path("rotated.pdf"), false).expect("rotate");
        assert!(!codes(&rotated.fidelity).contains(&"annotations-dropped".to_string()));
        assert!(codes(&rotated.fidelity).contains(&"acro-form-dropped".to_string()));

        let cropped = set_page_boxes(&source, "2", [0.0, 0.0, 200.0, 300.0], &workspace.path("cropped.pdf"), false)
            .expect("crop");
        assert!(!codes(&cropped.fidelity).contains(&"annotations-dropped".to_string()));
    }

    #[test]
    fn split_reports_the_catalog_and_left_out_annotations_of_each_part() {
        let workspace = Workspace::new();
        let source = workspace.write("complex.pdf", &complex_pdf());
        let report = split(&source, "1,2", &workspace.path("parts")).expect("split");

        assert_eq!(report.documents.len(), 2);
        let first = &report.documents[0];
        assert_eq!(first.pages, 1);
        assert_eq!(
            codes(&first.fidelity),
            ["acro-form-dropped", "destinations-dropped", "catalog-entry-dropped", "outlines-dropped"],
            "the kept page carries the only annotation"
        );

        let second = &report.documents[1];
        let annotation = second.fidelity.iter().find(|loss| loss.code == FidelityCode::AnnotationsDropped).expect("page loss");
        assert_eq!((annotation.source, annotation.page), (0, Some(1)));
        assert!(codes(&second.fidelity).contains(&"outlines-dropped".to_string()));
    }

    #[test]
    fn split_of_a_plain_document_reports_no_losses() {
        let workspace = Workspace::new();
        let source = workspace.create("plain.pdf", &["One", "Two", "Three"]);
        let report = split_every(&source, 2, &workspace.path("every")).expect("split every");

        assert_eq!(report.documents.len(), 2);
        for part in &report.documents {
            assert!(part.fidelity.is_empty(), "plain part lost something: {:?}", part.fidelity);
        }
    }

    #[test]
    fn corrupt_references_fail_without_producing_half_output() {
        let workspace = Workspace::new();
        let broken = workspace.write(
            "broken-annots.pdf",
            &assemble(&[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Annots [99 0 R] >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] >>",
            ]),
        );
        let out = workspace.path("never.pdf");
        let plan = PagePlan { sources: vec![broken.clone()], placements: vec![PagePlacement::Source { source: 0, page: 1 }] };

        let error = compose(&plan, &out, true).expect_err("a missing annotation object must fail");
        assert_eq!(error.code(), "format/invalid");
        assert!(!out.exists(), "a failed compose must not leave an output file");

        let error = arrange(&broken, &workspace.path("never-arrange.pdf"), &DocumentArrange::default()).expect_err("corrupt");
        assert_eq!(error.code(), "format/invalid");

        let error = delete_pages(&broken, "2", &workspace.path("never-delete.pdf"), true).expect_err("corrupt survivor");
        assert_eq!(error.code(), "format/invalid");
    }

    #[test]
    fn unreadable_documents_report_invalid_without_panicking() {
        let workspace = Workspace::new();
        let junk = workspace.write("junk.pdf", b"not a PDF at all");
        let empty_catalog = workspace.write(
            "no-pages.pdf",
            &assemble(&["<< /Type /Catalog >>"]),
        );

        for path in [&junk, &empty_catalog] {
            let plan = PagePlan {
                sources: vec![path.clone()],
                placements: vec![PagePlacement::Source { source: 0, page: 1 }],
            };
            let error = compose(&plan, &workspace.path("o1.pdf"), true).expect_err("unreadable input");
            assert_eq!(error.code(), "format/invalid");
            let error = merge(&[path.clone()], &workspace.path("o2.pdf"), true, None).expect_err("unreadable input");
            assert_eq!(error.code(), "format/invalid");
            let error = rotate_pages(path, "1", 90, &workspace.path("o3.pdf"), true).expect_err("unreadable input");
            assert_eq!(error.code(), "format/invalid");
        }
    }

    /// Guards that the loss codes serialize to the stable kebab-case spelling.
    #[test]
    fn loss_codes_serialize_stably() {
        let loss = FidelityLoss {
            code: FidelityCode::OutlinesDropped,
            source: 1,
            page: None,
            entry: "/Outlines".to_string(),
            message: "gone".to_string(),
        };
        let value = serde_json::to_value(&loss).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "code": "outlines-dropped",
                "source": 1,
                "page": null,
                "entry": "/Outlines",
                "message": "gone",
            })
        );
    }
}
