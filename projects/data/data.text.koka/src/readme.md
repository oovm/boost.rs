# 🛠️ Koka Parser Developer Guide

Koka support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-koka`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-koka = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern Koka with data classes, coroutines, and lambdas:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_koka::{KokaBuilder, KokaLanguage};

let language = KokaLanguage::new();
let builder = KokaBuilder::new(&language);
let mut session = ParseSession::<KokaLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Koka specific constructs like data classes, sealed classes, suspend functions, lambdas, and property delegates.

### 2. Incremental Parsing
Koka projects (especially Android or KMP projects) can be massive. `data-text-koka` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_koka::{KokaBuilder, KokaLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = KokaLanguage::new();
let builder = KokaBuilder::new(&language);
let mut session = ParseSession::<KokaLanguage>::new(32);
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

### 3. KMP and Multiplatform Support
The parser correctly handles Koka Multiplatform (KMP) specific syntax like `expect`/`actual` declarations, ensuring accurate trees across all targets.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Koka source text, supporting complex string templates, backticked identifiers, and various numeric literal formats.
- **Parser**: A high-performance syntax analyzer that handles Koka's expressive grammar, including generics, coroutines, and modern language features.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various Koka versions and edge cases.
