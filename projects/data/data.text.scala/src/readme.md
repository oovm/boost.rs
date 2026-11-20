# 🛠️ Scala Parser Developer Guide

Scala support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-scala`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-scala = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Scala source files, supporting Scala 2 and Scala 3 syntax:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_scala::{ScalaBuilder, ScalaLanguage};

let language = ScalaLanguage::new();
let builder = ScalaBuilder::new(&language);
let mut session = ParseSession::<ScalaLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Scala specific constructs like package clauses, imports, object/class/trait definitions, and method declarations.

### 2. Incremental Parsing
Scala projects can be large. `data-text-scala` supports sub-millisecond incremental updates for efficient IDE feedback:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_scala::{ScalaBuilder, ScalaLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = ScalaLanguage::new();
let builder = ScalaBuilder::new(&language);
let mut session = ParseSession::<ScalaLanguage>::new(32);
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
The parser is designed for industrial-grade fault tolerance, recovering gracefully from missing braces or malformed declarations to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Scala source text, supporting Scala 3's indentation-based syntax and Scala 2's traditional syntax.
- **Parser**: A high-performance recursive descent parser handling complex Scala features like implicits, generics, and pattern matching.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
