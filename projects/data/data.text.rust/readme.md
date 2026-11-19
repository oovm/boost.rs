# 🚀 Rust Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-rust.svg)](https://crates.io/crates/data-text-rust)
[![Documentation](https://docs.rs/data-text-rust/badge.svg)](https://docs.rs/data-text-rust)

**Safety and Speed for the System.** `data-text-rust` is a high-performance, incremental Rust parser built on the `data.text` framework. Engineered for the next generation of Rust developer tools, IDEs, and static analyzers, it delivers sub-millisecond performance for massive codebases.

## ✨ Core Features

- **⚡ Blazing Fast**: Built in Rust for sub-millisecond parsing, essential for real-time feedback in complex crates.
- **🔄 Truly Incremental**: Re-parse only what changed. Ideal for massive workspaces and responsive IDE feedback.
- **🛡️ Industrial-Grade Fault Tolerance**: Gracefully handles syntax errors, providing precise diagnostics for Rust code.
- **🌳 High-Fidelity AST**: Captures every detail of Rust, including traits, lifetimes, macros, and trivia.
- **Full Rust Support**: Comprehensive coverage of traits, generics, and ownership patterns.
- **IDE Ready**: Designed from the ground up for Language Server Protocol (LSP) and MCP integration.
- **Lossless Syntax**: Retains all source details, making it perfect for automated refactoring and formatting.

## 🏗️ Modern Architecture

Leveraging the **Green/Red Tree** pattern (similar to Roslyn and rust-analyzer):
1. **Efficient Immutability**: Share nodes across tree versions without expensive copies.
2. **Type-Safe API**: Easy traversal with strongly-typed nodes.
3. **Zero-Copy Trivia**: Efficiently manage comments and whitespace.

## 🤝 Contributing

We welcome contributions! Whether it's bug reports, feature requests, or code, help us build the best Rust parsing infrastructure. Check our [issues](https://github.com/oovm/boost.rs/issues) or submit a PR.
