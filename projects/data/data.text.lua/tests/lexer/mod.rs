use data_text_lua::{LuaLanguage, LuaLexer};
use language_testing::lexing::LexerTester;
use std::{path::Path, time::Duration};

#[test]
fn test_lua_lexer() -> Result<(), data_text_core::TextError> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = here.join("tests/lexer");
    let language = LuaLanguage::default();
    let lexer = LuaLexer::new(&language);
    let test_runner = LexerTester::new(tests).with_extension("lua").with_timeout(Duration::from_secs(5));
    test_runner.run_tests(&lexer)
}
