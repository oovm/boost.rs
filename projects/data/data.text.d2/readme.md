# 🚀 D2 Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-d2.svg)](https://crates.io/crates/data-text-d2)
[![Documentation](https://docs.rs/data-text-d2/badge.svg)](https://docs.rs/data-text-d2)

**Visualizing Code, Incremental by Nature** — A high-performance, incremental D2 diagramming language parser built on the `data.text` framework. Optimized for modern architecture documentation, developer workflows, and real-time visualization tools.

## 🎯 Project Vision

D2 (Declarative Diagramming) is designed to make diagrams as easy to write as code. `data-text-d2` brings the power of the `data.text` framework to the D2 ecosystem, providing a robust, high-performance infrastructure for parsing and analyzing D2 files. By utilizing incremental parsing capabilities, it enables developers to build highly responsive visualization tools and IDE extensions that update diagrams in real-time as you type. Whether you are building live documentation portals, automated architecture generators, or sophisticated diagram editors, `data-text-d2` provides the efficient foundation you need.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance to provide sub-millisecond parsing, essential for real-time visualization feedback.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale architecture diagrams.
- **🌳 High-Fidelity AST**: Generates a clean and easy-to-traverse Abstract Syntax Tree capturing:
    - **Shapes & Connections**: Precise mapping of shapes, containers, and the relationships between them.
    - **Attributes & Styling**: Detailed tracking of styling properties, labels, and metadata.
    - **Nested Containers**: Comprehensive management of nested structures and groupings.
- **🛡️ Robust Error Recovery**: Engineered to handle incomplete or malformed code gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active diagramming.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent diagram structure discovery.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.

## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
