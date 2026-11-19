# 🚀 Delphi Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-delphi.svg)](https://crates.io/crates/data-text-delphi)
[![Documentation](https://docs.rs/data-text-delphi/badge.svg)](https://docs.rs/data-text-delphi)

**Legacy Power, Modern Performance** — A high-performance, incremental Delphi/Object Pascal parser built on the `data.text` framework. Optimized for large-scale enterprise applications, legacy codebase modernization, and real-time developer tooling.

## 🎯 Project Vision

Delphi (Object Pascal) has been a mainstay of enterprise application development for decades, with millions of lines of code still in active use. `data-text-delphi` aims to provide a robust, modern, Rust-powered infrastructure for parsing Delphi that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated migration utilities that can handle massive Delphi projects in real-time. Whether you are building custom linters, refactoring tools, or sophisticated IDE extensions, `data-text-delphi` provides the high-fidelity AST and efficiency needed to tame the complexity of Delphi codebases.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Delphi projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only modified sections of large source files. Ideal for large-scale projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Delphi/Object Pascal:
    - **Units & Programs**: Precise mapping of `unit`, `interface`, `implementation`, and `program` blocks.
    - **Object-Oriented**: Full support for classes, interfaces, inheritance, and method overloading.
    - **Properties & Events**: Detailed tracking of property declarations and event handling mechanisms.
    - **Generics & Attributes**: Robust handling of modern Delphi features like generics and custom attributes.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code structure discovery.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Delphi source files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
