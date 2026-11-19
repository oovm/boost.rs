use data_text_bash::{BashLanguage, BashParser};
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_bash_parser() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = BashLanguage::default();
    let parser = BashParser::new(&language);
    let test_runner = ParserTester::new(here.join("tests/parser")).with_extension("sh").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
