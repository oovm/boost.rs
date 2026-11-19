use data_text_php::{language::PhpLanguage, lexer::PhpLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_php_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = PhpLanguage::default();
    let lexer = PhpLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("php").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
