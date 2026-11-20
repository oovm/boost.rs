//! Language-service contract tests for TypeScript.

use core::range::Range;
use data_text_core::vfs::MemoryVfs;
use language_server::service::LanguageService;
use language_typescript::lsp::TypescriptLanguageService;

const URI: &str = "memory:///main.ts";

const SOURCE: &str = "const answer = 40;\nfunction add(a: number, b: number) { return a + b; }\nconsole.log(add(answer, 2));\n";

fn service() -> TypescriptLanguageService<MemoryVfs> {
    TypescriptLanguageService::new(MemoryVfs::new())
}

fn offset_of(haystack: &str, needle: &str, occurrence: usize) -> usize {
    let mut search_from = 0;
    let mut found = None;
    for _ in 0..=occurrence {
        let index = haystack[search_from..].find(needle).expect("needle must exist") + search_from;
        found = Some(index);
        search_from = index + needle.len();
    }
    found.expect("needle must exist")
}

#[tokio::test]
async fn hover_describes_a_function_declaration() {
    let service = service();
    service.did_open(URI, SOURCE).await;

    let offset = offset_of(SOURCE, "function", 0);
    let hover = service.hover(URI, Range { start: offset, end: offset }).await.expect("hover must be available");

    assert!(hover.contents.contains("TypeScript Function"), "unexpected hover contents: {}", hover.contents);
    assert_eq!(hover.range.map(|range| range.start), Some(offset));
}

#[tokio::test]
async fn definition_resolves_a_module_binding_reference() {
    let service = service();
    service.did_open(URI, SOURCE).await;

    let reference = offset_of(SOURCE, "answer", 1);
    let definitions = service.definition(URI, Range { start: reference, end: reference + "answer".len() }).await;

    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].range.start, offset_of(SOURCE, "answer", 0));
    assert_eq!(definitions[0].range.end - definitions[0].range.start, "answer".len());
}

#[tokio::test]
async fn references_list_the_declaration_and_its_uses() {
    let service = service();
    service.did_open(URI, SOURCE).await;

    let declaration = offset_of(SOURCE, "answer", 0);
    let references = service.references(URI, Range { start: declaration, end: declaration + "answer".len() }).await;

    let starts: Vec<usize> = references.iter().map(|location| location.range.start).collect();
    assert_eq!(starts, vec![offset_of(SOURCE, "answer", 0), offset_of(SOURCE, "answer", 1)]);
}

#[tokio::test]
async fn formatting_rewrites_the_whole_buffer() {
    let service = service();
    let source = "const a   =   1";
    service.did_open(URI, source).await;

    let edits = service.formatting(URI).await;

    assert_eq!(edits.len(), 1, "expected a single full-buffer edit");
    assert_eq!(edits[0].range, Range { start: 0, end: source.len() });
    assert_ne!(edits[0].new_text, source);
}
