use data_text_wgsl::{WgslLanguage, WgslLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_wgsl_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = WgslLanguage::default();
    let lexer = WgslLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("wgsl").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
