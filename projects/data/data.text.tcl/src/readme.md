# 🛠️ Tcl Parser Developer Guide

Tcl support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-tcl`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-tcl = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Tcl scripts, supporting commands, variables, and complex command substitutions:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_tcl::{TclBuilder, TclLanguage};

let language = TclLanguage::new();
let builder = TclBuilder::new(&language);
let mut session = ParseSession::<TclLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Tcl specific constructs like commands, arguments, variable references, and scripts within braces.

### 2. Incremental Parsing
Tcl scripts are often used in embedded systems or as configuration. `data-text-tcl` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_tcl::{TclBuilder, TclLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = TclLanguage::new();
let builder = TclBuilder::new(&language);
let mut session = ParseSession::<TclLanguage>::new(32);
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
The parser is designed for industrial-grade fault tolerance, recovering gracefully from unclosed braces or quotes to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Tcl source text, supporting Tcl's unique "everything is a string" philosophy, command substitutions, and variable expansions.
- **Parser**: A high-performance recursive descent parser that handles Tcl's command-based structure and nested scripts.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
