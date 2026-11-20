# 🛠️ Bash Parser Developer Guide

Bash support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-bash`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-bash = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a Bash script with functions, loops, and redirections:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_bash::{BashBuilder, BashLanguage};

let language = BashLanguage::new();
let builder = BashBuilder::new(&language);
let mut session = ParseSession::<BashLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Bash specific constructs like function definitions, command pipelines, variable expansions, and redirections.

### 2. Incremental Parsing
Bash scripts can sometimes be long and complex. `data-text-bash` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_bash::{BashBuilder, BashLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = BashLanguage::new();
let builder = BashBuilder::new(&language);
let mut session = ParseSession::<BashLanguage>::new(32);
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

### 3. Expansion Awareness
The parser identifies different types of shell expansions (parameter, command, arithmetic), providing a solid foundation for building advanced static analysis tools.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Bash source text, handling complex rules for word splitting, quoting (single, double, ANSI-C), and comment identification.
- **Parser**: A high-performance syntax analyzer that handles Bash's flexible grammar, including control structures and redirections.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various Bash syntax edge cases.
