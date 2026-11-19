# 🚀 data-text-tcl

[![Crates.io](https://img.shields.io/crates/v/data-text-tcl.svg)](https://crates.io/crates/data-text-tcl)
[![Documentation](https://docs.rs/data-text-tcl/badge.svg)](https://docs.rs/data-text-tcl)

**Dynamic Scripting with Industrial Precision** — A high-performance, incremental Tcl parser built on the `data.text` framework. Optimized for Tcl's unique "everything is a string" philosophy, complex command substitutions, and responsive developer tools.

## 🎯 Project Vision

Tcl (Tool Command Language) is a powerful, dynamic language widely used in EDA, networking, and embedded systems. `data-text-tcl` aims to provide a robust, modern, Rust-powered infrastructure for parsing Tcl that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle complex Tcl scripts in real-time. Whether you are building custom linters, automated test generation tools, or sophisticated IDE extensions for Tcl/Tk projects, `data-text-tcl` provides the high-fidelity AST and efficiency needed to support the Tcl community.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Tcl projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale Tcl scripts where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise syntax tree capturing the full depth of Tcl:
    - **Command Substitution**: Precise mapping of `[...]` and `$...` constructs.
    - **Dynamic Scoping**: Detailed tracking of procedures, variables, and namespaces.
    - **Grouping Rules**: Robust parsing of braces `{...}` and double quotes `"..."`.
    - **Indentation & Formatting**: Precise capture of indentation and whitespace for faithful code refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

`data-text-tcl` follows the modern Green/Red Tree architecture (inspired by Roslyn):

- **Green Tree**: Immutable, lossless, and syntax-only tree. It captures the full fidelity of the source code, including trivia (comments, whitespace).
- **Red Tree**: A facade over the Green Tree that provides a convenient, type-safe API for tree traversal and analysis, including parent pointers and absolute offsets.

This design enables efficient incremental parsing and powerful refactoring capabilities.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
