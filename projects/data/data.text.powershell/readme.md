# 🚀 PowerShell Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-powershell.svg)](https://crates.io/crates/data-text-powershell)
[![Documentation](https://docs.rs/data-text-powershell/badge.svg)](https://docs.rs/data-text-powershell)

**Automation Mastery with Rust Efficiency** — A high-performance, incremental PowerShell parser built on the `data.text` framework. Optimized for system administration, DevOps automation, and modern IDE integration for the PowerShell language.

## 🎯 Project Vision

PowerShell is a powerful task automation and configuration management framework from Microsoft. `data-text-powershell` aims to provide a robust, modern, Rust-powered infrastructure for parsing PowerShell that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, code analysis tools, and automated refactoring utilities that can handle complex PowerShell scripts and modules in real-time. Whether you are building custom linters, automated script generators, or sophisticated IDE extensions for PowerShell Core or Windows PowerShell, `data-text-powershell` provides the high-fidelity AST and efficiency needed to support the PowerShell ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large PowerShell projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale automation projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of PowerShell:
    - **Commands & Cmdlets**: Precise mapping of command invocations, parameters, and arguments.
    - **Variables & Scoping**: Detailed tracking of PowerShell's complex variable scoping and data types.
    - **Control Flow**: Robust parsing of pipeline expressions, loops, and conditional statements.
    - **Modules & Functions**: Support for module definitions, function declarations, and advanced script blocks.
    - **Comments & Whitespace**: Retains all trivia, enabling faithful round-trip processing and refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of PowerShell files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
