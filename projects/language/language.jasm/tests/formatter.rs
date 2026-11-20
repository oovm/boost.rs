//! Formatter tests for JASM.

use data_text_core::{Parser, SourceText};
use data_text_jasm::{JasmBuilder, JasmLanguage, JasmLexer, JasmParser};

/// Test code formatter.
#[test]
fn test_formatter() {
    let source = SourceText::new(
        r#"
.class public Test
.version 65:0

.method public static main:"([Ljava/lang/String;)V"
return
.end method
"#,
    );

    let language = JasmLanguage::default();
    let parser = JasmParser::new(&language);
    let mut cache = data_text_core::parser::ParseSession::default();

    let result = parser.parse(&source, &[], &mut cache);
    assert!(result.result.is_ok());

    let _green = result.result.unwrap();
    {
        use data_text_core::Builder;
        use language_jasm::JasmFormatter;
        let builder = JasmBuilder::new(&language);
        let mut build_cache = data_text_core::parser::ParseSession::default();
        let typed = builder.build(&source, &[], &mut build_cache);
        let root = typed.result.expect("jasm typed root");
        let formatter = JasmFormatter::new();
        let formatted = formatter.format(&root);
        assert!(!formatted.is_empty());

        let pretty_formatted = formatter.format_pretty(&root);
        assert!(!pretty_formatted.is_empty());
    }
}
