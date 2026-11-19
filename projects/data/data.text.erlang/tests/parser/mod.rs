use data_text_core::parser::{ParseSession, Parser};
use data_text_erlang::{language::ErlangLanguage, parser::ErlangParser};

#[test]
fn test_erlang_parser() {
    let lang = ErlangLanguage::new();
    let parser = ErlangParser::new(&lang);
    let source = "-module(hello).\n-export([hello/0]).\nhello() -> ok.";
    let mut session = ParseSession::<ErlangLanguage>::new(16);
    let output = parser.parse(source, &[], &mut session);
    assert!(output.result.is_ok());
    let green = output.result.unwrap();
    assert!(!green.children.is_empty());
}
