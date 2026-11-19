# 🛠️ TOML Parser Developer Guide

Toml support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-toml`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-toml = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a TOML file:

```rust
use data_text_toml::parse;

fn main() {
    let code = r#"
        [package]
        name = "data-text-toml"
        version = "0.1.0"
        authors = ["Yggdrasil <contact@yggdrasil.com>"]

        [dependencies]
        serde = { version = "1.0", features = ["derive"] }
    "#;

    match parse(code) {
        Ok(root) => println!("Parsing successful! Top-level items: {}", root.items.len()),
        Err(error) => eprintln!("Errors found during parsing: {error}"),
    }
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract TOML tables, keys, and values.

### 2. Incremental Parsing
No need to re-parse massive `Cargo.lock` or configuration files when small changes occur:
```text
// Assuming you have an old parse result 'old_result' and new source text 'new_source'
let new_result = parser.reparse(&new_source, &old_result);
```

### 3. Diagnostics
`data-text-toml` provides precise error feedback for malformed TOML, such as invalid date formats, re-defined keys, or unclosed strings:
```text
for diag in result.diagnostics() {
    println!("[{}:{}] {}", diag.line, diag.column, diag.message);
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes TOML source text into a stream of tokens, handling various string types, numbers, dates, and table headers.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle TOML's hierarchical structure and nested key-value pairs.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance TOML analysis, formatting, and validation tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of standard TOML features and edge cases.
