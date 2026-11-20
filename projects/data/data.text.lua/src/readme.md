# 🛠️ Lua Parser Developer Guide

Lua support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-lua`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-lua = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Lua scripts, including support for tables, functions, and control flow:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_lua::{LuaBuilder, LuaLanguage};

let language = LuaLanguage::new();
let builder = LuaBuilder::new(&language);
let mut session = ParseSession::<LuaLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Lua specific constructs like function definitions, table constructors, local variable declarations, and loops.

### 2. Incremental Parsing
Lua scripts can grow large in game development. `data-text-lua` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_lua::{LuaBuilder, LuaLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = LuaLanguage::new();
let builder = LuaBuilder::new(&language);
let mut session = ParseSession::<LuaLanguage>::new(32);
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
The parser is designed for industrial-grade fault tolerance, recovering gracefully from missing `end` keywords or malformed table syntax to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Lua source text, supporting Lua-specific long strings (`[[ ... ]]`), long comments, and numeric literals.
- **Parser**: A high-performance recursive descent parser with Pratt parsing for expressions, handling Lua's operator precedence and table constructors.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
