# 📖 Go Parser User Guide

Go support for the `data.text` language framework.

This guide helps you integrate `data-text-go` into your project and perform common parsing tasks efficiently.

## 🚀 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing a Go file:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_go::{GoBuilder, GoLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
package main

import "fmt"

func main() {
    fmt.Println("Hello from Boost Go!")
}
"#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = GoLanguage::new();
    let builder = GoBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<GoLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    assert!(result.diagnostics.is_empty(), "unexpected diagnostics: {:#?}", result.diagnostics);
    let root = result.result.expect("parse result");
    assert_eq!(root.package.as_deref(), Some("main"));
    assert_eq!(root.imports.len(), 1);
    assert_eq!(root.declarations.len(), 1);
    println!("Parsing successful!");
}
```

## 🔍 Core Functionality

### 1. Syntax Tree Traversal
After a successful parse, use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Go constructs like package declarations, struct methods, or channel operations.

### 2. Incremental Parsing
Optimize performance by only re-parsing changed sections:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_go::{GoBuilder, GoLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = GoLanguage::new();
let builder = GoBuilder::new(&language);
let mut session = ParseSession::<GoLanguage>::new(32);
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
`data-text-go` provides detailed error contexts tailored for Go developers:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_go::{GoBuilder, GoLanguage};

let language = GoLanguage::new();
let builder = GoBuilder::new(&language);
let mut session = ParseSession::<GoLanguage>::new(32);
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

