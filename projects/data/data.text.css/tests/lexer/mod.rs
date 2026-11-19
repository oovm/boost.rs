use data_text_css::{CssLanguage, CssLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_css_lexer() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = CssLanguage::default();
    let lexer = CssLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("css").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
