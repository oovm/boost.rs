# 🛠️ C# Parser Developer Guide

C# support for the `data.text` language framework.

This crate provides lexing, parsing, and AST building for the C# language,
integrated into the `data.text` family. It supports modern C# features including
namespaces, classes, structs, records, and more.

This guide is designed to help you quickly get started with developing and integrating `data-text-csharp`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-csharp = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern C# with records, LINQ, and primary constructors:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_csharp::{CSharpBuilder, CSharpLanguage};

let language = CSharpLanguage::new();
let builder = CSharpBuilder::new(&language);
let mut session = ParseSession::<CSharpLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract C# specific constructs like records, primary constructors, async methods, LINQ expressions, and attributes.

### 2. Incremental Parsing
C# solutions can contain thousands of files. `data-text-csharp` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_csharp::{CSharpBuilder, CSharpLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = CSharpLanguage::new();
let builder = CSharpBuilder::new(&language);
let mut session = ParseSession::<CSharpLanguage>::new(32);
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

### 3. Roslyn Compatibility
While built in Rust, the architecture is inspired by Roslyn (Green/Red Trees), making it familiar for developers coming from the .NET ecosystem and enabling high-performance analysis tools.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes C# source text, supporting verbatim strings, interpolated strings, raw string literals, and various numeric formats.
- **Parser**: A high-performance syntax analyzer that handles C#'s complex grammar, including modern 12.0+ features and legacy constructs.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various C# versions and edge cases.
