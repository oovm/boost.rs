# 🚀 data-text-swift

[![Crates.io](https://img.shields.io/crates/v/data-text-swift.svg)](https://crates.io/crates/data-text-swift)
[![Documentation](https://docs.rs/data-text-swift/badge.svg)](https://docs.rs/data-text-swift)

**Safety and Speed for Apple Ecosystem Development** — A high-performance, incremental Swift parser built on the `data.text` framework. Optimized for Swift 5.10+ features, complex generic systems, and highly responsive developer tools.

## 🎯 Project Vision

Swift is the modern foundation for Apple platform development, and its evolution brings sophisticated features like property wrappers, result builders, and complex concurrency models. `data-text-swift` aims to provide a robust, premium, Rust-powered infrastructure for parsing Swift that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive Swift projects in real-time. Whether you are building custom linters, automated code generation tools, or sophisticated IDE extensions, `data-text-swift` provides the high-fidelity AST and efficiency needed to keep pace with the continuous growth of the Swift language.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Swift projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only what has changed. Ideal for large-scale Swift apps where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise syntax tree capturing the full depth of modern Swift:
    - **Modern Features**: Full support for Property Wrappers, Result Builders, Variadic Generics, and Macros.
    - **Concurrency**: Deep integration of `async`/`await`, `actors`, and `Sendable` protocol-related constructs.
    - **Advanced Type System**: Robust handling of complex generic constraints, opaque types (`some`), and existential types (`any`).
    - **Functional & OOP**: Detailed mapping of Closures, Enums with associated values, Protocols, and Extensions.
    - **SwiftUI Support**: Precise parsing of declarative UI syntax patterns common in SwiftUI projects.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent code discovery and analysis.

## 🏗️ Architecture

`data-text-swift` follows the modern Green/Red Tree architecture (inspired by Roslyn):

- **Green Tree**: Immutable, lossless, and syntax-only tree. It captures the full fidelity of the source code, including trivia (comments, whitespace).
- **Red Tree**: A facade over the Green Tree that provides a convenient, type-safe API for tree traversal and analysis, including parent pointers and absolute offsets.

This design enables efficient incremental parsing and powerful refactoring capabilities.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
