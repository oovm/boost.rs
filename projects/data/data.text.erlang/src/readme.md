# 🛠️ data-text-erlang Developer Guide

Erlang support for the `data.text` language framework.

Welcome to the internal implementation of the Erlang parser. This module provides the core logic for tokenizing and parsing Erlang source code into a high-fidelity syntax tree.

## 📦 Core Components

- **Lexer**: Tokenizes Erlang source code, handling atoms, variables, and macros with full fidelity.
- **Parser**: Implements the Erlang grammar, producing a Green Tree that represents the concrete syntax.
- **AST**: Provides a type-safe Red Tree facade for easy traversal and analysis.
- **Language**: Defines the `ErlangLanguage` configuration and integration with the `data.text` framework.

## 🚀 Usage Example

### Basic Parsing

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_erlang::{ErlangBuilder, ErlangLanguage};

fn main() {
    // 1. Prepare source code
    let code = "let x = 1;";
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = ErlangLanguage::new();
    let builder = ErlangBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<ErlangLanguage>::new(32);
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
use data_text_erlang::{ErlangBuilder, ErlangLanguage};

fn main() {
    // 1. Prepare source code
    let code = "let x = 1;";
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = ErlangLanguage::new();
    let builder = ErlangBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<ErlangLanguage>::new(32);
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
use data_text_erlang::{ErlangBuilder, ErlangLanguage};

let language = ErlangLanguage::new();
let builder = ErlangBuilder::new(&language);
let mut session = ParseSession::<ErlangLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```
