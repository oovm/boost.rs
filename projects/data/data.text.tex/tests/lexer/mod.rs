use language_testing::lexing::LexerTester;
use data_text_tex::{language::TexLanguage, lexer::TexLexer};
use std::{path::Path, time::Duration};

#[test]
fn test_tex_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = TexLanguage::default();
    let lexer = TexLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("tex").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
