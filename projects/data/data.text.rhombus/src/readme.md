# 🛠️ Scheme Parser Developer Guide

Rhombus support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-scheme`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-scheme = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Scheme scripts, supporting various R7RS and traditional Lisp constructs:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_rhombus::{RhombusBuilder, RhombusLanguage};

let language = RhombusLanguage::new();
let builder = RhombusBuilder::new(&language);
let mut session = ParseSession::<RhombusLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Scheme specific constructs like lists, vectors, symbols, and procedure definitions.

### 2. Incremental Parsing
Scheme scripts can be part of larger systems. `data-text-scheme` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_rhombus::{RhombusBuilder, RhombusLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = RhombusLanguage::new();
let builder = RhombusBuilder::new(&language);
let mut session = ParseSession::<RhombusLanguage>::new(32);
{
    let source = SourceText::new("a");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("b");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. Error Recovery
The parser is designed for industrial-grade fault tolerance, recovering gracefully from mismatched parentheses or malformed expressions to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Scheme source text, supporting standard S-expressions, symbols, strings, and numeric literals.
- **Parser**: A high-performance recursive descent parser optimized for the nested nature of Lisp-like languages.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
