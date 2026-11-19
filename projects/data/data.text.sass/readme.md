# 🚀 Sass Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-sass.svg)](https://crates.io/crates/data-text-sass)
[![Documentation](https://docs.rs/data-text-sass/badge.svg)](https://docs.rs/data-text-sass)

**Style with Speed and Precision** — A high-performance, incremental Sass (indented syntax) parser built on the `data.text` framework. Optimized for modern CSS workflows, design systems, and real-time styling tools.

## 🎯 Project Vision

Sass revolutionized web styling with its powerful features and indented syntax. `data-text-sass` aims to provide a robust, modern, Rust-powered infrastructure for parsing Sass that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, style analyzers, and automated refactoring utilities that can handle complex design systems in real-time. Whether you are building custom linters, automated theme generators, or sophisticated IDE extensions for Sass, `data-text-sass` provides the high-fidelity AST and efficiency needed to support the modern styling ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time style analysis.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale design systems where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Sass:
    - **Indented Syntax**: Precise tracking of indentation-based nesting and scoping.
    - **Variables & Mixins**: Detailed mapping of variable declarations, mixin definitions, and includes.
    - **Control Flow**: Robust parsing of `@if`, `@for`, `@each`, and `@while` directives.
    - **Functions & Operators**: Detailed tracking of custom functions and complex mathematical expressions.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active styling.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent style discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Sass files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
