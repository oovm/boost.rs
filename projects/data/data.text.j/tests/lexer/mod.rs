use data_text_j::{language::JLanguage, lexer::JLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_J_lexer() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = JLanguage::default();
    let lexer = JLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("J").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
