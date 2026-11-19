# 🚀 C# Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-csharp.svg)](https://crates.io/crates/data-text-csharp)
[![Documentation](https://docs.rs/data-text-csharp/badge.svg)](https://docs.rs/data-text-csharp)

**Enterprise-Grade Power and Speed for .NET** — A high-performance, incremental C# parser built on the `data.text` framework. Optimized for modern C# (12.0+) features, Roslyn-inspired architecture, and high-responsiveness developer tools.

## 🎯 Project Vision

C# is the cornerstone of the .NET ecosystem, and its rapid evolution brings complex syntax and powerful language features. `data-text-csharp` aims to provide a robust, modern, Rust-powered infrastructure for parsing C# that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive C# solutions in real-time. Whether you are building custom linters, automated code generators, or sophisticated IDE extensions, `data-text-csharp` provides the high-fidelity AST and efficiency needed to keep pace with C#'s continuous growth.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large C# projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale C# solutions where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of modern C#:
    - **Modern Features**: Full support for Records, Pattern Matching, Init-only properties, and Primary Constructors.
    - **Asynchronous Programming**: Deep integration of `async`, `await`, and `IAsyncEnumerable` constructs.
    - **LINQ**: Precise parsing of Query Expressions and method-based LINQ syntax.
    - **Generics & Attributes**: Robust handling of complex generic constraints and attribute-based metadata.
    - **Unsafe Code**: Support for parsing unsafe blocks and pointer operations where necessary.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
