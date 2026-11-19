# 🚀 language-macro

[![Crates.io](https://img.shields.io/crates/v/language-macro.svg)](https://crates.io/crates/language-macro)
[![Documentation](https://docs.rs/language-macro/badge.svg)](https://docs.rs/language-macro)

**Procedural Macros for `data.text`** — Derive macros and helper attributes for `data.text` language implementations.

## 🎯 Why language-macro?

Procedural macros reduce boilerplate in language implementations. `language-macro` provides derive macros and helper attributes that simplify implementing `data.text` traits.

## ✨ Key Features

- **🔧 Derive Macros** — Automatically implement common `data.text` traits
- **📝 Code Generation** — Generate repetitive code patterns
- **🎯 Type-Safe** — Compile-time verification of generated code
- **⚡ Zero Runtime Cost** — Macros expand at compile time

## 🏗️ Architecture

### Available Macros

| Macro | Purpose |
|-------|---------|
| `#[derive(Language)]` | Auto-implement the `Language` trait |
| `json!` | JSON literal construction |

## 🔗 Ecosystem Integration

Used by language parser implementations, test code generation, and AST construction utilities.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-macro).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
