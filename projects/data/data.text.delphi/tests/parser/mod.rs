use data_text_core::errors::TextError;
use data_text_delphi::{DelphiLanguage, DelphiParser};
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_delphi_parser() -> Result<(), TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = DelphiLanguage::default();
    let parser = DelphiParser::new(&language);
    let test_runner = ParserTester::new(here.join("tests/parser")).with_extension("pas").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
