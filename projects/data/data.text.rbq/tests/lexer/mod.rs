use data_text_rbq::{RbqLanguage, RbqLexer};
use language_testing::lexing::LexerTester;
use std::time::Duration;

#[test]
#[cfg(feature = "serde")]
fn test_rbq_lexer() -> Result<(), data_text_core::TextError> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let config = RbqLanguage::default();
    let lexer = RbqLexer::new(&config);
    let tester = LexerTester::new(tests).with_extension("rbq").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
