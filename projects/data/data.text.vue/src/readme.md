# 🛠️ Vue Parser Developer Guide

Vue support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-vue`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-vue = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing Vue Single File Components (SFCs), supporting template, script, and style blocks:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_vue::{VueBuilder, VueLanguage};

let language = VueLanguage::default();
let builder = VueBuilder::new();
let mut session = ParseSession::<VueLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Vue specific constructs like top-level blocks, template directives, script setup variables, and scoped styles.

### 2. Incremental Parsing
Vue SFCs are often edited in IDEs. `data-text-vue` supports sub-millisecond incremental updates for a fluid editing experience:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_vue::{VueBuilder, VueLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = VueLanguage::default();
let builder = VueBuilder::new();
let mut session = ParseSession::<VueLanguage>::new(32);
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
The parser is designed for industrial-grade fault tolerance, recovering gracefully from unclosed tags or malformed script blocks to provide continuous feedback in IDEs.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Vue SFC source text, handling the transitions between HTML-like template syntax, JavaScript/TypeScript in script blocks, and CSS/SCSS in style blocks.
- **Parser**: A high-performance parser that manages the coordination between different languages within a single SFC.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for details on our snapshot-based testing.
