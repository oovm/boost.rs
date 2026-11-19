# 🚀 Structurizr Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-structurizr.svg)](https://crates.io/crates/data-text-structurizr)
[![Documentation](https://docs.rs/data-text-structurizr/badge.svg)](https://docs.rs/data-text-structurizr)

**Architecture as Code, Optimized** — A high-performance, incremental Structurizr DSL parser built on the `data.text` framework. Optimized for the C4 model, software architecture documentation, and modern developer toolchains.

## 🎯 Project Vision

Structurizr DSL is a powerful way to define software architecture based on the C4 model. `data-text-structurizr` provides a modern, Rust-powered infrastructure for parsing and analyzing Structurizr DSL files with extreme efficiency. By leveraging incremental parsing capabilities, it enables the creation of highly responsive architecture modeling tools that update diagrams and documentation instantly as you type. Our goal is to provide the most robust and efficient foundation for architects and developers to manage their software models as code.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance to provide sub-millisecond parsing for large architecture models.
- **🔄 Incremental by Design**: Built-in support for partial updates—re-parse only the sections of the DSL that changed.
- **🌳 High-Fidelity AST**: Generates a comprehensive Abstract Syntax Tree capturing the full Structurizr DSL:
    - **C4 Model Elements**: Precise mapping of people, software systems, containers, and components.
    - **Relationships**: Detailed tracking of interactions and dependencies between elements.
    - **Views & Styles**: Comprehensive management of diagram views, filtering, and styling directives.
    - **Includes & Extensions**: Robust handling of workspace inclusion and DSL extensions.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—essential for live modeling environments.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent architecture discovery.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.

## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
