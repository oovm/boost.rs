# 🚀 Zig Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-zig.svg)](https://crates.io/crates/data-text-zig)
[![Documentation](https://docs.rs/data-text-zig/badge.svg)](https://docs.rs/data-text-zig)

**Pragmatic Performance for Systems Programming** — A high-performance, incremental Zig parser built on the `data.text` framework. Specially optimized for Zig's unique syntax, compile-time features, and modern developer tool integration.

## 🎯 Project Vision

Zig is designed for robustness, optimality, and maintainability. `data-text-zig` brings these same values to the world of Zig code analysis. By leveraging incremental parsing architecture, we provide a modern, Rust-powered infrastructure that can handle Zig's complex compile-time features (`comptime`) and unique syntax with sub-millisecond responsiveness. Our goal is to empower developers to build sophisticated IDEs, build systems, and static analyzers that can navigate large Zig projects with ease, ensuring that the simplicity and power of Zig are matched by its tooling.

## ✨ Core Features

- **⚡ Blazing Fast**: Engineered in Rust to deliver sub-millisecond parsing performance, essential for real-time developer feedback and large-scale Zig project analysis.
- **🔄 Incremental by Design**: Only re-parse what changed. Ideal for the rapid iteration cycles typical of Zig development.
- **🌳 High-Fidelity AST**: Generates a comprehensive Abstract Syntax Tree capturing the full depth of Zig:
    - **Comptime Awareness**: Precise tracking of compile-time expressions and logic.
    - **Error Handling**: Detailed mapping of error sets, `try`, and `catch` blocks.
    - **Structs & Unions**: Robust support for Zig's data layout and memory management constructs.
    - **C Integration**: Seamlessly handles Zig's C interoperability syntax.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and other `data.text`-based code analysis tools.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
