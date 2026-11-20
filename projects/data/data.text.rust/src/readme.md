# 📖 Rust Parser User Guide

Rust support for the `data.text` language framework.

This guide helps you integrate `data-text-rust` into your project and perform common parsing tasks efficiently.

## 🚀 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing a Rust function with traits and generics:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_rust::{RustBuilder, RustLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        pub trait Summary {
            fn summarize(&self) -> String;
        }

        pub struct Article {
            pub headline: String,
            pub content: String,
        }

        impl Summary for Article {
            fn summarize(&self) -> String {
                format!("{}...", &self.headline[0..20])
            }
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = RustLanguage::new();
    let builder = RustBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<RustLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🔍 Core Functionality

### 1. Syntax Tree Traversal
After a successful parse, use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Rust constructs like trait definitions, struct fields, or async blocks.

### 2. Incremental Parsing
Optimize performance by only re-parsing changed sections:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_rust::{RustBuilder, RustLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = RustLanguage::new();
let builder = RustBuilder::new(&language);
let mut session = ParseSession::<RustLanguage>::new(32);
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

### 3. Diagnostics & Error Recovery
`data-text-rust` provides detailed error contexts tailored for Rust developers:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_rust::{RustBuilder, RustLanguage};

let language = RustLanguage::new();
let builder = RustBuilder::new(&language);
let mut session = ParseSession::<RustLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🛠️ Performance & Reliability

- **High-Fidelity AST**: Retains all trivia (whitespace and comments), making it ideal for code formatting and refactoring tools.
- **Fault Tolerance**: Automatically recovers from syntax errors to provide as much information as possible from the rest of the file.
- **Memory Efficiency**: Leverages immutable data structures (Green Trees) for low-overhead tree management.


