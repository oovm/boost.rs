use data_text_ocaml::{OCamlLanguage, OCamlParser};
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_ocaml_parser() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = OCamlLanguage::default();
    let parser = OCamlParser::new(&language);
    let test_runner = ParserTester::new(here.join("tests/parser")).with_extension("ml").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
