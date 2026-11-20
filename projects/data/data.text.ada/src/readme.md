# 🛠️ Ada Parser Developer Guide

Ada support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-ada`.

## 🚦 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing an Ada package specification:

```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ada::{AdaLanguage, AdaParser};

fn main() {
    // 1. Prepare source code
    let code = r#"
        package Hello_World is
           procedure Say_Hello;
        end Hello_World;
    "#;
    let source = SourceText::new(code);

    // 2. Initialize parser
    let language = AdaLanguage::new();
    let parser = AdaParser::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<AdaLanguage>::new(32);
    let result = parser.parse(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Ada constructs like package declarations, procedure signatures, or tasking statements.

### 2. Incremental Parsing
No need to re-parse the entire file when small changes occur:
```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ada::{AdaLanguage, AdaParser};

// 复用 `ParseSession` 携带旧树：第二次 parse 只重解析变化区域
let language = AdaLanguage::new();
let parser = AdaParser::new(&language);
let mut session = ParseSession::<AdaLanguage>::new(32);
{
    let source = SourceText::new("package a is end a;");
    let result = parser.parse(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("package b is end b;");
    let result = parser.parse(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. Diagnostics
`data-text-ada` provides rich error contexts specifically tailored for Ada developers:
```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ada::{AdaLanguage, AdaParser};

let language = AdaLanguage::new();
let parser = AdaParser::new(&language);
let mut session = ParseSession::<AdaLanguage>::new(32);
let source = SourceText::new("package ;");
let result = parser.parse(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Ada source text into a stream of tokens, handling keywords (case-insensitive), operators, and numeric literals.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle Ada's structural declarations and expression precedence.
- **AST**: A strongly-typed syntax abstraction layer designed for building high-performance Ada analysis tools and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various Ada edge cases and language versions.
