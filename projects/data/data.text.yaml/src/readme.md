# 🛠️ YAML Parser Developer Guide

Yaml support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-yaml`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-yaml = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a YAML document:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_yaml::{YamlBuilder, YamlLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        version: "3.8"
        services:
          web:
            image: "nginx:latest"
            ports:
              - "80:80"
          db:
            image: "postgres:13"
            environment:
              POSTGRES_PASSWORD: example
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = YamlLanguage::new();
    let builder = YamlBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<YamlLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract YAML mappings, sequences, scalars, and handle anchors/aliases.

### 2. Incremental Parsing
No need to re-parse massive YAML configuration files when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_yaml::{YamlBuilder, YamlLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = YamlLanguage::new();
let builder = YamlBuilder::new(&language);
let mut session = ParseSession::<YamlLanguage>::new(32);
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

### 3. Diagnostics
`data-text-yaml` provides precise error feedback for malformed YAML, such as indentation errors, unmatched brackets, or invalid tag usage:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_yaml::{YamlBuilder, YamlLanguage};

let language = YamlLanguage::new();
let builder = YamlBuilder::new(&language);
let mut session = ParseSession::<YamlLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes YAML source text into a stream of tokens, handling indentation, block vs. flow styles, and various scalar formats.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle YAML's hierarchical structure and complex value types.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance YAML analysis, formatting, and validation tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various YAML features and edge cases.
