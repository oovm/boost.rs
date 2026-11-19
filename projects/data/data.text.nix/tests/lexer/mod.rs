use data_text_nix::{NixLanguage, NixLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_nix_lexer() -> Result<(), data_text_core::OakError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = NixLanguage::default();
    let lexer = NixLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("nix").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
