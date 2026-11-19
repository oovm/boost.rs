use data_text_core::TextError;
use data_text_typst::{language::TypstLanguage, lexer::TypstLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_typst_lexer() -> Result<(), TextError> {
    let language = TypstLanguage::default();
    let lexer = TypstLexer::new(&language);
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let test_dir = here.join("tests/lexer");
    let tester = LexerTester::new(test_dir).with_extension("typ").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
