use data_text_msil::{MsilLanguage, MsilParser};
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_msil_parser() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = MsilLanguage::default();
    let parser = MsilParser::new(&language);
    let test_runner = ParserTester::new(here.join("tests/files")).with_extension("il").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
