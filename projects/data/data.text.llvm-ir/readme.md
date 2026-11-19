# 🚀 LLVM IR Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-llvm-ir.svg)](https://crates.io/crates/data-text-llvm-ir)
[![Documentation](https://docs.rs/data-text-llvm-ir/badge.svg)](https://docs.rs/data-text-llvm-ir)

**The Backbone of Modern Compiler Infrastructure** — A high-performance, incremental LLVM Intermediate Representation (IR) parser built on the `data.text` framework. Optimized for compiler middle-end analysis, optimization passes, and toolchain development.

## 🎯 Project Vision

LLVM IR is the universal language of modern compiler technology, serving as the bridge between high-level source languages and low-level machine code. `data-text-llvm-ir` aims to provide a robust, modern, Rust-powered infrastructure for parsing LLVM IR that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive compiler tools, static analyzers, and visualization utilities that can handle massive IR files with sub-millisecond latency. Whether you are building custom optimization passes, security auditors, or sophisticated IDE extensions for compiler engineers, `data-text-llvm-ir` provides the high-fidelity AST and efficiency needed to interact with LLVM IR at scale.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-throughput compiler pipelines and real-time analysis of large IR files.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only the functions or blocks that changed. Ideal for iterative compiler development and interactive optimization tools.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of LLVM IR:
    - **Module Structure**: Precise mapping of Global Variables, Function Definitions, and Declarations.
    - **Basic Blocks**: Detailed tracking of Control Flow Graph (CFG) nodes and edges.
    - **Instruction Set**: Robust parsing of all LLVM instructions, including SSA form variables and types.
    - **Metadata & Attributes**: Full support for debug metadata, function attributes, and calling conventions.
    - **Comments & Trivia**: Retains all comments and formatting, enabling faithful round-trip processing and analysis.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for debugging generated IR or malformed inputs.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of LLVM IR files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
