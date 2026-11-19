# 🚀 WAT Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-wat.svg)](https://crates.io/crates/data-text-wat)
[![Documentation](https://docs.rs/data-text-wat/badge.svg)](https://docs.rs/data-text-wat)

**WebAssembly Text Format Parsing Made Simple** — A high-performance, incremental WAT parser built on the `data.text` framework.

## 🎯 Why data-text-wat?

The WebAssembly Text Format (WAT) is the human-readable representation of WebAssembly binaries. `data-text-wat` provides industrial-grade parsing support for WebAssembly development, debugging, and analysis.

## ✨ Key Features

- **⚡ Blazing Fast** — Sub-millisecond parsing for build tools and IDEs
- **🔄 Incremental Parsing** — Re-parse only what changed
- **🌳 High-Fidelity AST** — Captures modules, components, functions, instructions, memory, tables, imports/exports
- **🛡️ Robust Error Recovery** — Precise diagnostics for debugging
- **🧩 Deep Ecosystem Integration** — Works with `language-server` and `language-mcp`

## 🏗️ Architecture

Follows the Green/Red Tree architecture with efficient immutability, lossless syntax trees, and type-safe traversal.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/data-text-wat).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
