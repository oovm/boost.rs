# 🛠️ data-text-fsharp Developer Guide

Fsharp support for the `data.text` language framework.

Welcome to the internal implementation of the F# parser. This module provides the core logic for tokenizing and parsing F# source code into a high-fidelity syntax tree.

## 📦 Core Components

- **Lexer**: Tokenizes F# source code, handling indentation-sensitive syntax, computation expressions, and active patterns with full fidelity.
- **Parser**: Implements the F# grammar, producing a Green Tree that represents the concrete syntax.
- **AST**: Provides a type-safe Red Tree facade for easy traversal and analysis.
- **Language**: Defines the `FSharpLanguage` configuration and integration with the `data.text` framework.

## 🚀 Usage Example

### Basic Parsing

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_fsharp::{FSharpBuilder, FSharpLanguage};

fn main() {
    // 1. Prepare source code
    let code = "let x = 1;";
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = FSharpLanguage::new();
    let builder = FSharpBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<FSharpLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

### Incremental Parsing

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_fsharp::{FSharpBuilder, FSharpLanguage};

fn main() {
    // 1. Prepare source code
    let code = "let x = 1;";
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = FSharpLanguage::new();
    let builder = FSharpBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<FSharpLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🔍 Diagnostics

The parser provides detailed diagnostics for syntax errors, including error ranges and helpful messages.

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_fsharp::{FSharpBuilder, FSharpLanguage};

let language = FSharpLanguage::new();
let builder = FSharpBuilder::new(&language);
let mut session = ParseSession::<FSharpLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```
