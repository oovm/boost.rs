# 🚀 Raku Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-raku.svg)](https://crates.io/crates/data-text-raku)
[![Documentation](https://docs.rs/data-text-raku/badge.svg)](https://docs.rs/data-text-raku)

**Expressivity Meets Efficiency** — A high-performance, incremental Raku (formerly Perl 6) parser built on the `data.text` framework. Optimized for advanced concurrency, grammar-driven programming, and modern IDE support for the Raku language.

## 🎯 Project Vision

Raku is a highly expressive and multi-paradigm programming language, designed for the next hundred years. `data-text-raku` brings modern parsing infrastructure to this powerful language, providing a high-performance, Rust-powered foundation for analysis and tooling. By utilizing incremental parsing capabilities, it enables the creation of highly responsive developer tools that can handle Raku's unique and complex grammar in real-time. Whether you are building sophisticated linters, automated refactoring tools, or feature-rich IDE extensions, `data-text-raku` provides the robust, efficient foundation required for the Raku ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing for complex Raku syntax.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Essential for large-scale Raku projects.
- **🌳 High-Fidelity AST**: Generates a clean and easy-to-traverse Abstract Syntax Tree capturing:
    - **Grammars & Regex**: First-class support for Raku's powerful grammar and regex systems.
    - **Concurrency & Junctions**: Precise mapping of Raku's unique concurrency constructs and junctions.
    - **Types & Signatures**: Detailed tracking of Raku's expressive type system and function signatures.
    - **Multi-dispatch**: Robust handling of multi-methods and multi-subs.
- **🛡️ Robust Error Recovery**: Engineered to handle incomplete or malformed code gracefully, providing precise diagnostics—crucial for a smooth developer experience.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for Model Context Protocol capabilities.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.

## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
