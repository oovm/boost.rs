# 🚀 JavaScript Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-javascript.svg)](https://crates.io/crates/data-text-javascript)
[![Documentation](https://docs.rs/data-text-javascript/badge.svg)](https://docs.rs/data-text-javascript)

**Performance and Flexibility for the Web Ecosystem** — A high-performance, incremental JavaScript parser built on the `data.text` framework. Optimized for modern ECMAScript (ES2022+) features, JSX support, and real-time developer tools.

## 🎯 Project Vision

JavaScript is the backbone of the modern web, with an ecosystem that moves faster than almost any other. `data-text-javascript` aims to provide a robust, modern, Rust-powered infrastructure for parsing JavaScript that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive JavaScript projects in real-time. Whether you are building custom linters, automated code migration tools, or sophisticated IDE extensions, `data-text-javascript` provides the high-fidelity AST and efficiency needed to keep pace with the continuous evolution of ECMAScript.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large web projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale JavaScript projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of modern JavaScript:
    - **Modern ECMAScript**: Full support for ES2022+ features, including classes, async/await, and optional chaining.
    - **JSX Support**: First-class support for parsing JSX syntax, essential for modern React and Vue development.
    - **Asynchronous Programming**: Deep integration of `async`, `await`, and `Promise` related constructs.
    - **Modules**: Robust handling of ESM (`import`/`export`) and CommonJS module systems.
    - **Decorators**: Support for experimental and proposed decorator syntax.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
