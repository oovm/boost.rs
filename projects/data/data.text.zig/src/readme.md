# 🛠️ Zig Parser Developer Guide

Zig support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-zig`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-zig = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a Zig source file:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_zig::{ZigBuilder, ZigLanguage};

let language = ZigLanguage::new();
let builder = ZigBuilder::new(&language);
let mut session = ParseSession::<ZigLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Zig constructs like function definitions, comptime blocks, or struct declarations.

### 2. Incremental Parsing
Zig projects often involve many small files or frequent edits. `data-text-zig` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_zig::{ZigBuilder, ZigLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = ZigLanguage::new();
let builder = ZigBuilder::new(&language);
let mut session = ParseSession::<ZigLanguage>::new(32);
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
The parser is designed for industrial-grade fault tolerance, recovering gracefully from missing semicolons or malformed `try/catch` blocks to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Zig source text, supporting Zig-specific features like multi-line strings, character literals, and preprocessor-like `@import`.
- **Parser**: A high-performance recursive descent parser with Pratt parsing for expressions, handling Zig's operator precedence and complex compile-time logic.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
