use data_text_c::{CParser, language::CLanguage};
use data_text_core::{Parser, Source, TextEdit, parser::ParseSession};
use std::rc::Rc;

fn main() {
    let code = include_str!("test_simple.c");
    let language = CLanguage::default();
    let parser = CParser::new(&language);
    let mut cache = data_text_core::parser::ParseSession::<CLanguage>::default();
    let source = code;
    let edits = &[];

    let result = parser.parse(source, edits, &mut cache);
    match result.result {
        Ok(output) => {
            println!("Parsing successful!");
            println!("Root node: {:?}", output);
        }
        Err(e) => {
            println!("Parsing failed: {:?}", e);
        }
    }
}
