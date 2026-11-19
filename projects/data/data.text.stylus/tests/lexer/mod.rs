use data_text_stylus::{language::StylusLanguage, lexer::StylusLexer};
use language_testing::lexing::LexerTester;
use std::time::Duration;

#[test]
fn test_stylus_lexer() -> Result<(), data_text_core::TextError> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let config = StylusLanguage::default();
    let lexer = StylusLexer::new(&config);
    let tester = LexerTester::new(tests).with_extension("styl").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
