use data_text_astro::{SvelteLanguage, SvelteLexer};
use data_text_core::errors::TextError;
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_astro_lexer() -> Result<(), TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let language = SvelteLanguage::default();
    let lexer = SvelteLexer::new(&language);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("svelte").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)?;
    Ok(())
}
