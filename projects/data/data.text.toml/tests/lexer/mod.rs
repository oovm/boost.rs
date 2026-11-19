use language_testing::lexing::LexerTester;
use data_text_toml::{language::TomlLanguage, lexer::TomlLexer};
use std::{path::Path, time::Duration};

#[test]
fn test_toml_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = TomlLanguage::default();
    let lexer = TomlLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("toml").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
