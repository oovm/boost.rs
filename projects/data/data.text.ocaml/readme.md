# 🚀 OCaml Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-ocaml.svg)](https://crates.io/crates/data-text-ocaml)
[![Documentation](https://docs.rs/data-text-ocaml/badge.svg)](https://docs.rs/data-text-ocaml)

**Functional Power with Pragmatic Performance** — A high-performance, incremental OCaml parser built on the `data.text` framework. Optimized for functional programming paradigms, strong type systems, and modern developer tooling.

## 🎯 Project Vision

OCaml is a powerful functional language known for its industrial-strength type system and pragmatic approach to performance. `data-text-ocaml` aims to provide a robust, modern, Rust-powered infrastructure for parsing OCaml that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated refactoring utilities that can handle complex OCaml projects in real-time. Whether you are building custom linters, automated code generators, or sophisticated IDE extensions, `data-text-ocaml` provides the high-fidelity AST and efficiency needed to keep pace with OCaml's expressive ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large OCaml projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale OCaml projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of OCaml:
    - **Functional Constructs**: Deep support for algebraic data types, pattern matching, and higher-order functions.
    - **Module System**: Precise mapping of signatures, structures, functors, and module inclusions.
    - **Object System**: Robust handling of OCaml's unique structural subtyping and object-oriented features.
    - **Polymorphic Variants**: Detailed tracking of polymorphic variants and their usage.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of OCaml files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
