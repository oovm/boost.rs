# 🚀 data-text-scala

[![Crates.io](https://img.shields.io/crates/v/data-text-scala.svg)](https://crates.io/crates/data-text-scala)
[![Documentation](https://docs.rs/data-text-scala/badge.svg)](https://docs.rs/data-text-scala)

**Scalable Parsing for a Scalable Language** — A high-performance, incremental Scala parser built on the `data.text` framework. Optimized for Scala 3 features, complex type systems, and responsive developer tools.

## 🎯 Project Vision

Scala is known for its sophisticated type system and fusion of object-oriented and functional programming. `data-text-scala` aims to provide a robust, modern, Rust-powered infrastructure for parsing Scala that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive Scala projects in real-time. Whether you are building custom linters, automated migration tools for Scala 2 to 3, or sophisticated IDE extensions, `data-text-scala` provides the high-fidelity AST and efficiency needed to keep pace with the evolving Scala ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Scala projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale Scala apps where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise syntax tree capturing the full depth of modern Scala:
    - **Scala 3 Support**: Full support for enums, opaque types, extension methods, and context parameters.
    - **Functional & OOP**: Detailed mapping of traits, classes, objects, and higher-order functions.
    - **Indentation Syntax**: Precise handling of Scala 3's significant indentation rules.
    - **Macros & Meta**: Robust parsing of inline methods and macro-related constructs.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

`data-text-scala` follows the modern Green/Red Tree architecture (inspired by Roslyn):

- **Green Tree**: Immutable, lossless, and syntax-only tree. It captures the full fidelity of the source code, including trivia (comments, whitespace).
- **Red Tree**: A facade over the Green Tree that provides a convenient, type-safe API for tree traversal and analysis, including parent pointers and absolute offsets.

This design enables efficient incremental parsing and powerful refactoring capabilities.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
