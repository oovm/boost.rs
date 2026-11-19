use data_text_hlsl::{HlslLanguage, HlslLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_hlsl_lexer() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let language = HlslLanguage::default();
    let lexer = HlslLexer::new(&language);
    let test_runner = LexerTester::new(tests).with_extension("hlsl").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
