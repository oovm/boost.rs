# 🚀 Pascal Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-pascal.svg)](https://crates.io/crates/data-text-pascal)
[![Documentation](https://docs.rs/data-text-pascal/badge.svg)](https://docs.rs/data-text-pascal)

**Structured Programming with Modern Speed** — A high-performance, incremental Pascal parser built on the `data.text` framework. Optimized for legacy codebase maintenance, educational tools, and modern IDE integration for the Pascal language.

## 🎯 Project Vision

Pascal is a cornerstone of structured programming, still widely used in education and specialized industrial applications. `data-text-pascal` aims to provide a robust, modern, Rust-powered infrastructure for parsing Pascal that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated refactoring utilities that can handle complex Pascal projects in real-time. Whether you are building custom linters, automated code migration tools, or sophisticated IDE extensions for Delphi or Free Pascal, `data-text-pascal` provides the high-fidelity AST and efficiency needed to support the Pascal ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Pascal projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale legacy projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Pascal:
    - **Procedures & Functions**: Precise mapping of subprogram definitions, parameters, and return types.
    - **Types & Variables**: Detailed tracking of record types, sets, arrays, and pointer types.
    - **Control Flow**: Robust parsing of `begin...end` blocks, loops, and conditional statements.
    - **Units & Modules**: Support for modular programming constructs and unit interfaces.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Pascal files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
