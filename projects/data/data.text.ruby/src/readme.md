# 🛠️ Ruby Parser Developer Guide

Ruby support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-ruby`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-ruby = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Ruby code, including support for classes, methods, and blocks:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_ruby::{RubyBuilder, RubyLanguage};

let language = RubyLanguage::new();
let builder = RubyBuilder::new(&language);
let mut session = ParseSession::<RubyLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Ruby specific constructs like class definitions, method bodies, block expressions, and complex literals.

### 2. Incremental Parsing
Ruby codebases (especially Rails apps) can be massive. `data-text-ruby` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_ruby::{RubyBuilder, RubyLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = RubyLanguage::new();
let builder = RubyBuilder::new(&language);
let mut session = ParseSession::<RubyLanguage>::new(32);
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

### 3. DSL and Metaprogramming
The parser is designed to handle Ruby's flexible nature, providing high-fidelity trees for code that heavily uses DSLs or dynamic method calls.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Ruby source text, including support for complex string interpolations, heredocs, and various percent literals.
- **Parser**: A high-performance syntax analyzer that handles Ruby's expression-heavy syntax and complex grammar rules.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various Ruby versions and edge cases.
