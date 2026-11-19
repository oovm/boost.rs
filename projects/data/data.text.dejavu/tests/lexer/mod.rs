use data_text_dejavu::{DejavuLanguage, DejavuLexer};
use language_testing::lexing::LexerTester;
use std::time::Duration;

#[test]
fn test_dejavu_lexer() -> Result<(), data_text_core::TextError> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let config = DejavuLanguage::default();
    let lexer = DejavuLexer::new(&config);
    let tester = LexerTester::new(tests).with_extension("dejavu").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
