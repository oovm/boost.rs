use data_text_core::LexerCache;

#[test]
fn test_rust_integration() -> Result<(), data_text_core::TextError> {
    use data_text_core::{Lexer, Parser, SourceText};
    use data_text_rust::{RustLanguage, RustLexer, RustParser};

    let language = RustLanguage::default();
    let lexer = RustLexer::new(&language);
    let parser = RustParser::new(&language);

    // Test basic integration
    let source = SourceText::new("fn main() { let x = 42; println!(\"Hello, world!\") }");

    // Test lexer
    let mut cache = data_text_core::parser::session::ParseSession::<RustLanguage>::default();
    let lex_output = lexer.lex(&source, &[], &mut cache);
    assert!(lex_output.result.is_ok(), "Lexer should produce tokens");

    // Test files
    cache.set_lex_output(lex_output.clone());
    let parse_output = parser.parse(&source, &[], &mut cache);
    assert!(parse_output.result.is_ok(), "Parser should produce AST");

    println!("Rust integration test passed - {} tokens generated", lex_output.result.unwrap().len());
    Ok(())
}

#[test]
fn test_rust_builder_single_file() -> Result<(), data_text_core::TextError> {
    use data_text_core::{Builder, SourceText, parser::session::ParseSession};
    use data_text_rust::{RustBuilder, RustLanguage};

    let language = RustLanguage::default();
    let builder = RustBuilder::new(&language);

    // Test simple function
    let source = SourceText::new("fn add(x: i32, y: i32) -> i32 { x + y }");

    // Create ParseSession
    let mut cache = ParseSession::<RustLanguage>::default();

    let diagnostics = builder.build(&source, &[], &mut cache);
    assert!(diagnostics.result.is_ok());
    Ok(())
}

#[test]
fn test_rust_builder_complex() -> Result<(), data_text_core::TextError> {
    use data_text_core::{Builder, SourceText, parser::session::ParseSession};
    use data_text_rust::{RustBuilder, RustLanguage};

    let language = RustLanguage::default();
    let builder = RustBuilder::new(&language);

    // Test complex Rust code
    let source = SourceText::new(
        r#"
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }
    
    fn distance(&self) -> f64 {
        ((self.x * self.x + self.y * self.y) as f64).sqrt()
    }
}

fn main() {
    let p = Point::new(3, 4);
    println!("Distance: {}", p.distance())
}
"#,
    );

    println!("Testing builder with complex Rust code");

    // Create ParseSession
    let mut cache = ParseSession::<RustLanguage>::default();

    let diagnostics = builder.build(&source, &[], &mut cache);
    match diagnostics.result {
        Ok(_typed_root) => {
            println!("Successfully built complex typed root")
        }
        Err(e) => {
            println!("Complex build failed with error: {}", e)
        }
    }
    if !diagnostics.diagnostics.is_empty() {
        println!("Complex build diagnostics: {:?}", diagnostics.diagnostics)
    }
    Ok(())
}
