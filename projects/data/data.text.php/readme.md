# 🚀 data-text-php

[![Crates.io](https://img.shields.io/crates/v/data-text-php.svg)](https://crates.io/crates/data-text-php)
[![Documentation](https://docs.rs/data-text-php/badge.svg)](https://docs.rs/data-text-php)

**Powering the Modern Web with Precision and Speed** — A high-performance, incremental PHP parser built on the `data.text` framework. Optimized for PHP 8.x features, complex legacy codebases, and real-time developer tools.

## 🎯 Project Vision

PHP continues to power a vast portion of the web, and its evolution from a simple scripting language to a modern, type-safe language brings significant complexity. `data-text-php` aims to provide a robust, modern, Rust-powered infrastructure for parsing PHP that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive PHP projects in real-time. Whether you are building custom linters, automated migration tools for PHP 8.3+, or sophisticated IDE extensions, `data-text-php` provides the high-fidelity AST and efficiency needed to keep pace with the modern PHP ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Fully utilizes Rust's performance and memory safety to achieve sub-millisecond parsing response times, essential for high-frequency analysis in large PHP applications.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. This is a massive advantage for large-scale PHP codebases where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise syntax tree capturing the full depth of modern PHP:
    - **Modern Features**: Full support for Attributes (annotations), Constructor Property Promotion, Union/Intersection Types, and Readonly Classes.
    - **Legacy Support**: Robust parsing of older PHP versions and mixed HTML/PHP files.
    - **Functional & OOP**: Detailed mapping of Anonymous Classes, Arrow Functions, Enums, and Traits.
    - **Asynchronous Flow**: Support for parsing modern async/fiber-related constructs.
    - **Indentation & Formatting**: Precise capture of indentation and whitespace for faithful code refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a fluid developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

`data-text-php` follows the modern Green/Red Tree architecture (inspired by Roslyn):

- **Green Tree**: Immutable, lossless, and syntax-only tree. It captures the full fidelity of the source code, including trivia (comments, whitespace).
- **Red Tree**: A facade over the Green Tree that provides a convenient, type-safe API for tree traversal and analysis, including parent pointers and absolute offsets.

This design enables efficient incremental parsing and powerful refactoring capabilities.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
