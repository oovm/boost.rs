use data_text_swift::{language::SwiftLanguage, lexer::SwiftLexer};
use language_testing::lexing::LexerTester;

use std::{path::Path, time::Duration};

#[test]
fn test_swift_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = SwiftLanguage::default();
    let lexer = SwiftLexer::new(&config);
    let test_runner = LexerTester::new(here.join("tests/lexer")).with_extension("swift").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
