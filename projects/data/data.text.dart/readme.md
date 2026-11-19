# 🚀 Dart Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-dart.svg)](https://crates.io/crates/data-text-dart)
[![Documentation](https://docs.rs/data-text-dart/badge.svg)](https://docs.rs/data-text-dart)

**Building the Future of UI with Precision** — A high-performance, incremental Dart parser built on the `data.text` framework. Optimized for Flutter development, modern Dart (3.0+) features, and high-responsiveness developer tools.

## 🎯 Project Vision

Dart is the language behind Flutter, and its growth has brought powerful features like patterns, records, and sound null safety. `data-text-dart` aims to provide a robust, modern, Rust-powered infrastructure for parsing Dart that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle large Flutter projects in real-time. Whether you are building custom linters, automated code generators, or sophisticated IDE extensions, `data-text-dart` provides the high-fidelity AST and efficiency needed to keep pace with Dart's continuous evolution.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Flutter projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale Dart projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of modern Dart:
    - **Modern Features**: Full support for Records, Patterns, Class modifiers, and Sealed classes.
    - **Asynchronous Programming**: Deep integration of `async`, `await`, and `Stream` constructs.
    - **Null Safety**: Precise parsing of null-safe syntax and type annotations.
    - **Generics & Annotations**: Robust handling of complex generic constraints and metadata annotations.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Dart scripts.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
