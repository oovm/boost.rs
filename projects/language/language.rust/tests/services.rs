#![feature(new_range_api)]

//! Language-service tests for Rust (formatter/highlighter).

mod formatter;
mod highlighter;

#[test]
fn test_complete_rust_program() -> Result<(), data_text_core::TextError> {
    use data_text_core::{Lexer, SourceText};
    use data_text_rust::{RustLanguage, RustLexer};
    use language_rust::{Highlighter, RustFormatter, RustHighlighter};

    let language = RustLanguage::default();
    let lexer = RustLexer::new(&language);
    let highlighter = RustHighlighter::new();
    let formatter = RustFormatter::new();

    let source = r#"
use std::collections::HashMap;

#[derive(Debug, Clone)]
struct Person {
    name: String,
    age: u32,
}

impl Person {
    fn new(name: String, age: u32) -> Self {
        Person { name, age }
    }
    
    fn greet(&self) -> String {
        format!("Hello, I'm {} and I'm {} years old", self.name, self.age)
    }
}

fn main() {
    let mut people = HashMap::new();
    let person = Person::new("Alice".to_string(), 30);
    people.insert(1, person);
    
    if let Some(p) = people.get(&1) {
        println!("{}", p.greet())
    }
}
"#;

    // Test lexer
    let source_text = SourceText::new(source);
    let mut cache = data_text_core::parser::session::ParseSession::<RustLanguage>::default();
    let lex_output = lexer.lex(&source_text, &[], &mut cache);
    assert!(lex_output.result.is_ok(), "Lexing should succeed");
    let tokens = lex_output.result.unwrap();
    assert!(!tokens.is_empty(), "Should tokenize complete program");

    // Test highlighter
    let highlights = highlighter.highlight(source);
    assert!(!highlights.is_empty(), "Should highlight complete program");

    // Test formatter
    let formatted = formatter.format(source);
    assert!(!formatted.is_empty(), "Should format complete program");

    println!("Complete Rust program test passed:");
    println!("  - {} tokens generated", tokens.len());
    println!("  - {} highlights generated", highlights.len());
    println!("  - Formatted code length: {} chars", formatted.len());
    Ok(())
}
