# 🛠️ Swift Parser Developer Guide

Swift support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-swift`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-swift = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern Swift with property wrappers, generics, and concurrency:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_swift::{SwiftBuilder, SwiftLanguage};

let language = SwiftLanguage::new();
let builder = SwiftBuilder::new(&language);
let mut session = ParseSession::<SwiftLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Swift specific constructs like property wrappers, actors, async functions, and result builders.

### 2. Incremental Parsing
Swift projects (especially those using SwiftUI) can be very complex. `data-text-swift` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_swift::{SwiftBuilder, SwiftLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = SwiftLanguage::new();
let builder = SwiftBuilder::new(&language);
let mut session = ParseSession::<SwiftLanguage>::new(32);
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

### 3. SwiftUI and DSL Handling
The parser is optimized to handle SwiftUI's declarative syntax and other result-builder-based DSLs, providing accurate trees for UI definitions and complex logic.

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Swift source text, including support for complex string literals (interpolations, multiline), custom operators, and various numeric formats.
- **Parser**: A high-performance syntax analyzer that handles Swift's sophisticated grammar, including generics, property wrappers, and modern concurrency features.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/readme.md](tests/readme.md) for handling of various Swift versions and edge cases.
