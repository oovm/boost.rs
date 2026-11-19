use data_text_core::TextError;
use language_testing::lexing::LexerTester;
use data_text_vala::{ValaLanguage, ValaLexer};
use std::{path::Path, time::Duration};

#[test]
fn test_vala_lexer() -> Result<(), TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = ValaLanguage::default();
    let lexer = ValaLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("vala").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}