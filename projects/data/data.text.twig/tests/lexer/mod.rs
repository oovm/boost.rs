use data_text_twig::{language::TwigLanguage, lexer::TwigLexer};
use language_testing::lexing::LexerTester;
use std::time::Duration;

#[test]
fn test_twig_lexer() -> Result<(), data_text_core::TextError> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let config = TwigLanguage::default();
    let lexer = TwigLexer::new(&config);
    let tester = LexerTester::new(tests).with_extension("twig").with_timeout(Duration::from_secs(5));
    tester.run_tests(&lexer)
}
