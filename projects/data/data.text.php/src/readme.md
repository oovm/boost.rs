# 🛠️ PHP Parser Developer Guide

Php support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-php`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-php = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern PHP with attributes, enums, and promoted properties:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_php::{PhpBuilder, PhpLanguage};

let language = PhpLanguage::new();
let builder = PhpBuilder::new(&language);
let mut session = ParseSession::<PhpLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract PHP specific constructs like class definitions, attributes, enums, promoted properties, and arrow functions.

### 2. Incremental Parsing
PHP projects (especially those using frameworks like Laravel or Symfony) can contain thousands of files. `data-text-php` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_php::{PhpBuilder, PhpLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = PhpLanguage::new();
let builder = PhpBuilder::new(&language);
let mut session = ParseSession::<PhpLanguage>::new(32);
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

### 3. Mixed Content Handling
The parser correctly handles mixed HTML and PHP files, identifying `<?php ... ?>` tags and maintaining the structural integrity of both the code and the surrounding template.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes PHP source text, supporting complex string interpolations (`{$var}`), heredocs, nowdocs, and various numeric literal formats.
- **Parser**: A high-performance syntax analyzer that handles PHP's complex grammar, including modern 8.x features and legacy constructs.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various PHP versions and edge cases.
