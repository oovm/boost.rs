# 🚀 Nix Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-nix.svg)](https://crates.io/crates/data-text-nix)
[![Documentation](https://docs.rs/data-text-nix/badge.svg)](https://docs.rs/data-text-nix)

**Declarative Configuration with Rust Efficiency** — A high-performance, incremental Nix parser built on the `data.text` framework. Optimized for reproducible builds, system configuration, and modern developer tooling for the Nix ecosystem.

## 🎯 Project Vision

Nix is a powerful functional language for system configuration and reproducible builds. `data-text-nix` aims to provide a robust, modern, Rust-powered infrastructure for parsing Nix that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated refactoring utilities that can handle complex Nix projects (like Nixpkgs) in real-time. Whether you are building custom linters, automated package generators, or sophisticated IDE extensions, `data-text-nix` provides the high-fidelity AST and efficiency needed to keep pace with the Nix ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Nix projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for massive Nix projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of Nix:
    - **Functional Constructs**: Deep support for attributes, sets, lists, and anonymous functions (lambdas).
    - **String Interpolation**: Precise mapping of `${ ... }` and double-quoted strings.
    - **Paths & URIs**: Robust handling of Nix-specific path types and URL literals.
    - **Let-In & With**: Detailed tracking of scoping constructs.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of Nix files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
