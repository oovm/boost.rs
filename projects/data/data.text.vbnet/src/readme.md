# VB.NET Parser for `data.text` framework

A comprehensive VB.NET parser implementation for the `data.text` framework, providing lexing, parsing, and AST generation capabilities.

## Features

- Complete VB.NET syntax support
- Lexer for tokenizing source code
- Parser for generating AST
- AST builder for converting green trees to high-level AST nodes
- Support for VB.NET-specific syntax constructs

## Usage

```rust
use data_text_vbnet::parse;

fn main() {
    let source = r#"
    Imports System

    Namespace MyNamespace
        Class MyClass
            Public Sub Hello()
                Console.WriteLine("Hello, World!")
            End Sub
        End Class
    End Namespace
    "#;

    match parse(source) {
        Ok(ast) => println!("Parsed successfully: {:?}", ast),
        Err(err) => println!("Parsing error: {:?}", err),
    }
}
```
