use data_text_core::errors::TextError;
use data_text_prolog::{language::PrologLanguage, parser::PrologParser};
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_prolog_parser() -> Result<(), TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/parser");
    let language = PrologLanguage::default();
    let parser = PrologParser::new(&language);
    let test_runner = ParserTester::new(tests).with_extension("pl").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
