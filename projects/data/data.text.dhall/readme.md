# 🚀 Dhall Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-dhall.svg)](https://crates.io/crates/data-text-dhall)
[![Documentation](https://docs.rs/data-text-dhall/badge.svg)](https://docs.rs/data-text-dhall)

**Programmable Configuration with Total Safety** — A high-performance, incremental Dhall parser built on the `data.text` framework. Optimized for programmable configuration, cloud-native infrastructure, and modern developer tooling.

## 🎯 Project Vision

Dhall is a programmable configuration language that is guaranteed to terminate and is completely safe to evaluate. `data-text-dhall` aims to provide a robust, modern, Rust-powered infrastructure for parsing Dhall that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive configuration editors, linting tools, and automated deployment pipelines that can handle complex Dhall graphs in real-time. Whether you are building custom configuration validators, cloud infrastructure generators, or sophisticated IDE extensions, `data-text-dhall` provides the high-fidelity AST and efficiency needed to harness the power of Dhall.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time configuration analysis.
- **🔄 Incremental by Design**: Built-in support for partial updates—re-parse only modified sections of large Dhall files. Ideal for complex configuration environments where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Dhall:
    - **Functional Constructs**: Full support for functions, let-bindings, and lambda expressions.
    - **Types & Records**: Precise mapping of Dhall's powerful type system, including records, unions, and optional types.
    - **Imports**: Robust handling of Dhall's unique import system (local files, URLs, environment variables).
    - **Built-in Functions**: Comprehensive support for Dhall's standard library and built-in operators.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active configuration editing.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent configuration discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Dhall files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
