use data_text_core::source::Source;
use data_text_rhombus::{language::RhombusLanguage, lexer::RhombusLexer};
use language_testing::lexing::LexerTester;
use std::time::Duration;

#[test]
fn test_rhombus_lexer() -> Result<(), data_text_core::TextError> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let config = RhombusLanguage::default();
    let lexer = RhombusLexer::new(&config);
    let tester = LexerTester::new(tests).with_extension("rhm").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
