# 🛠️ data-text-dart Developer Guide

Dart support for the `data.text` language framework.

Welcome to the internal implementation of the Dart parser. This module provides the core logic for tokenizing and parsing Dart source code into a high-fidelity syntax tree.

## 📦 Core Components

- **Lexer**: Tokenizes Dart source code, handling comments, strings, and identifiers with full fidelity.
- **Parser**: Implements the Dart grammar, producing a Green Tree that represents the concrete syntax.
- **AST**: Provides a type-safe Red Tree facade for easy traversal and analysis.
- **Language**: Defines the `DartLanguage` configuration and integration with the `data.text` framework.

## 🚀 Usage Example

### Basic Parsing

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_dart::{DartLanguage, builder::DartBuilder};

fn parse_dart_code(code: &str) {
    let source = SourceText::new(code);
    let language = DartLanguage::new();
    let builder = DartBuilder::new(&language);
    let mut session = ParseSession::<DartLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    }
}
```

### Incremental Parsing

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_dart::{DartLanguage, builder::DartBuilder};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = DartLanguage::new();
let builder = DartBuilder::new(&language);
let mut session = ParseSession::<DartLanguage>::new(32);
{
    let source = SourceText::new("var a = 1;");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("var a = 2;");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

## 🔍 Diagnostics

The parser provides detailed diagnostics for syntax errors, including error ranges and helpful messages.

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_dart::{DartLanguage, builder::DartBuilder};

let language = DartLanguage::new();
let builder = DartBuilder::new(&language);
let mut session = ParseSession::<DartLanguage>::new(32);
let source = SourceText::new("var a = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```
