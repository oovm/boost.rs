# 🚀 CMD Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-cmd.svg)](https://crates.io/crates/data-text-cmd)
[![Documentation](https://docs.rs/data-text-cmd/badge.svg)](https://docs.rs/data-text-cmd)

**Windows Batch Script Analysis** — A high-performance, incremental Windows batch script parser built on the `data.text` framework.

## 🎯 Why data-text-cmd?

Windows batch scripts (.bat, .cmd) remain essential for Windows automation and system administration. `data-text-cmd` provides modern parsing infrastructure for linting tools, IDE support, and analysis utilities.

## ✨ Key Features

- **⚡ Blazing Fast** — Sub-millisecond parsing for IDE integration
- **🔄 Incremental Parsing** — Re-parse only what changed
- **🌳 High-Fidelity AST** — Captures commands, control flow, variables, redirection
- **🛡️ Robust Error Recovery** — Precise diagnostics for common issues
- **🧩 Deep Ecosystem Integration** — Works with `language-server` and `language-mcp`

## 🏗️ Architecture

Follows the Green/Red Tree architecture with efficient immutability, lossless syntax trees, and type-safe traversal.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/data-text-cmd).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
