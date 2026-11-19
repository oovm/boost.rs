use data_text_groovy::{language::GroovyLanguage, lexer::GroovyLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_groovy_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = GroovyLanguage::default();
    let lexer = GroovyLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("groovy").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
