use data_text_astro::{SvelteLanguage, SvelteParser};
use data_text_core::errors::TextError;
use language_testing::parsing::ParserTester;
use std::{path::Path, time::Duration};

#[test]
fn test_astro_parser() -> Result<(), TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/parser");
    let language = SvelteLanguage::default();
    let parser = SvelteParser::new(&language);
    let test_runner = ParserTester::new(tests).with_extension("svelte").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&parser)
}
