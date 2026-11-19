# 🚀 Julia Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-julia.svg)](https://crates.io/crates/data-text-julia)
[![Documentation](https://docs.rs/data-text-julia/badge.svg)](https://docs.rs/data-text-julia)

**Numerical Computing Power with Rust Efficiency** — A high-performance, incremental Julia parser built on the `data.text` framework. Optimized for scientific computing, data analysis pipelines, and modern IDE support for the Julia language.

## 🎯 Project Vision

Julia is renowned for its high performance in numerical and scientific computing. `data-text-julia` aims to provide a robust, modern, Rust-powered infrastructure for parsing Julia that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated refactoring utilities that can handle complex Julia projects in real-time. Whether you are building custom linters, performance analyzers, or sophisticated IDE extensions, `data-text-julia` provides the high-fidelity AST and efficiency needed to keep pace with Julia's dynamic and high-performance ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in scientific computing environments.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale Julia projects where performance and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Julia:
    - **Multiple Dispatch**: Deep support for function definitions and multiple dispatch signatures.
    - **Metaprogramming**: Robust handling of macros, symbols, and expressions.
    - **Type System**: Precise mapping of Julia's expressive type system, including parametric types.
    - **Mathematical Constructs**: First-class support for Julia's unique mathematical syntax and operators.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Julia scripts.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
