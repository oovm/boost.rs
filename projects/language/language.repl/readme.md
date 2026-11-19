# 🚀 language-repl

[![Crates.io](https://img.shields.io/crates/v/language-repl.svg)](https://crates.io/crates/language-repl)
[![Documentation](https://docs.rs/language-repl/badge.svg)](https://docs.rs/language-repl)

**Interactive REPL Framework for Languages** — A terminal-based Read-Eval-Print Loop framework with multi-line support and real-time syntax highlighting.

## 🎯 Why language-repl?

REPLs are essential for interactive development, experimentation, and learning. `language-repl` provides a complete, terminal-based REPL framework that integrates deeply with language features.

## ✨ Key Features

- **🖥️ Terminal Integration** — Cross-platform terminal handling with raw mode support
- **📝 Multi-Line Input** — Automatic detection of incomplete statements
- **🎨 Syntax Highlighting** — Real-time highlighting via `language-highlight` integration
- **⌨️ Rich Editing** — Cursor movement, backspace, and auto-indentation
- **🔌 Customizable Handlers** — `ReplHandler` trait for language-specific behavior
- **🛡️ Error Recovery** — Graceful handling with continuation support

## 🏗️ Architecture

- `ReplHandler` — Trait for implementing language-specific REPL behavior
- `Repl` — Main REPL engine managing terminal interaction
- `LineBuffer` — Multi-line text input with cursor positioning

## 🔗 Ecosystem Integration

Integrates with `language-highlight` for syntax highlighting and any language parser implementing `ReplHandler`.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-repl).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
