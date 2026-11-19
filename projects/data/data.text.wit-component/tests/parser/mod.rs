use data_text_core::{parser::Parser, source::SourceText};
use data_text_wit_component::{WitLanguage, WitParser};

#[test]
fn test_parser_basic() {
    let input = "package example:test; world my-world {}";
    let source = SourceText::new(input);
    let language = WitLanguage::default();
    let parser = WitParser::new(&language);

    let mut session = data_text_core::parser::session::ParseSession::<WitLanguage>::default();
    let diagnostics = parser.parse(&source, &[], &mut session);

    assert!(diagnostics.result.is_ok())
}
