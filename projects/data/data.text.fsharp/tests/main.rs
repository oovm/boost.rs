use data_text_core::{Lexer, SourceText};
use data_text_fsharp::{FSharpLanguage, FSharpLexer};

#[test]
fn test_fsharp_lexer_integration() {
    let source = SourceText::new("let add x y = x + y\nlet result = add 5 3");
    let language = FSharpLanguage::default();
    let lexer = FSharpLexer::new(&language);

    let mut cache = data_text_core::parser::session::ParseSession::<FSharpLanguage>::default();
    let result = lexer.lex(&source, &[], &mut cache);

    match result.result {
        Ok(tokens) => {
            println!("F# lexer tests passed! Parsed {} tokens", tokens.len());
            assert!(!tokens.is_empty(), "Should parse at least one token")
        }
        Err(e) => panic!("F# lexer tests failed: {:?}", e),
    }
}
