# 🚀 TOML Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-toml.svg)](https://crates.io/crates/data-text-toml)
[![Documentation](https://docs.rs/data-text-toml/badge.svg)](https://docs.rs/data-text-toml)

**Precision and Performance for Configuration** — A high-performance, incremental TOML parser built on the `data.text` framework. Optimized for Rust project management, configuration files, and high-fidelity document processing.

## 🎯 Project Vision

TOML is the preferred configuration language for the Rust ecosystem, but its precise rules for strings, dates, and nested tables require a robust and accurate parser. `data-text-toml` provides a high-performance, Rust-powered infrastructure for parsing TOML that is both standard-compliant and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, configuration editors, and build tools that can handle large `Cargo.toml` files and complex configuration graphs in real-time.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's zero-cost abstractions to deliver sub-millisecond parsing, essential for real-time validation and large-scale configuration analysis.
- **🔄 Incremental by Design**: Built-in support for partial updates—re-parse only the sections of the TOML file that changed. Ideal for real-time editing of large project manifests.
- **🌳 High-Fidelity AST**: Generates a comprehensive Abstract Syntax Tree capturing the full depth of TOML:
    - **Tables & Arrays**: Precise mapping of standard tables, inline tables, and arrays of tables.
    - **Keys & Values**: Detailed tracking of simple, quoted, and dotted keys, along with all TOML value types.
    - **Dates & Times**: Native support for parsing RFC3339 date-time formats.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience when editing project configurations.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent project metadata discovery.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
