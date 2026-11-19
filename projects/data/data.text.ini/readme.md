# 🚀 INI Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-ini.svg)](https://crates.io/crates/data-text-ini)
[![Documentation](https://docs.rs/data-text-ini/badge.svg)](https://docs.rs/data-text-ini)

**Simplicity in Configuration** — A high-performance, incremental INI parser built on the `data.text` framework. Optimized for system configuration, legacy settings migration, and modern IDE support for the INI file format.

## 🎯 Project Vision

The INI format is one of the oldest and most widely used configuration formats, known for its simple section-based structure. `data-text-ini` provides a modern, Rust-powered infrastructure for analyzing and manipulating configuration files with extreme efficiency. By utilizing incremental parsing capabilities, it enables the creation of highly responsive configuration tools that can handle massive setting files with sub-millisecond updates. Whether you are building system configuration managers, automated settings migration tools, or sophisticated property editors, `data-text-ini` provides the robust, efficient foundation you need for high-fidelity configuration extraction.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to parse large configuration files with sub-millisecond latency.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only modified sections or keys. Ideal for real-time configuration monitoring and interactive editing.
- **🌳 High-Fidelity AST**: Generates a precise Abstract Syntax Tree capturing the full structure of INI:
    - **Sections**: Comprehensive mapping of named sections and global properties.
    - **Keys & Values**: Detailed tracking of key-value pairs, including support for various assignment operators.
    - **Comments**: Robust handling of both semicolon (`;`) and hash (`#`) style comments.
- **🛡️ Industrial-Grade Fault Tolerance**: Gracefully recovers from syntax errors, providing clear and actionable diagnostics—critical for maintaining a smooth user experience during configuration editing.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent configuration discovery and structural analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.

## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
