# 🚀 language-hover

[![Crates.io](https://img.shields.io/crates/v/language-hover.svg)](https://crates.io/crates/language-hover)
[![Documentation](https://docs.rs/language-hover/badge.svg)](https://docs.rs/language-hover)

**Hover Information Provider for the `data.text` family** — A lightweight, trait-based library for providing hover information in editors and IDEs.

## 🎯 Why language-hover?

Hover information is essential for modern developer experience — it provides instant feedback about symbols, types, and documentation without leaving the editor. `language-hover` provides a simple, unified interface for implementing hover support in any `data.text`-based language parser.

## ✨ Key Features

- **🎯 Trait-Based Design** — Clean `HoverProvider` trait for language-specific hover logic
- **📝 Markdown Support** — Rich formatting for documentation and type information
- **📍 Range-Aware** — Optional range information for precise hover targeting
- **🔄 Serde Integration** — Easy serialization for LSP implementations
- **🧩 Zero Dependencies** — Minimal footprint with optional serde support

## 🏗️ Architecture

The crate provides two main types:
- `Hover` — Represents hover content with optional range
- `HoverProvider<L>` — Trait for implementing language-specific hover

## 🔗 Ecosystem Integration

Works seamlessly with `language-server` for full Language Server Protocol support and any editor that displays hover information.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-hover).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
