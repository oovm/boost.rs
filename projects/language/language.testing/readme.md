# 🚀 language-testing

[![Crates.io](https://img.shields.io/crates/v/language-testing.svg)](https://crates.io/crates/language-testing)
[![Documentation](https://docs.rs/language-testing/badge.svg)](https://docs.rs/language-testing)

**Testing Utilities for `data.text` Parsers** — Helpers and macros for testing `data.text` language implementations.

## 🎯 Why language-testing?

Testing parsers requires specialized utilities for comparing syntax trees, checking error recovery, and validating incremental parsing. `language-testing` provides these tools for straightforward and maintainable test writing.

## ✨ Key Features

- **🧪 Parse Testing** — Helpers for parsing and validating syntax trees
- **📊 Snapshot Testing** — Compare parsed output against expected snapshots
- **🔍 Error Checking** — Validate error recovery and diagnostic output
- **⚡ Incremental Testing** — Test incremental parsing behavior
- **📝 Macro Support** — Convenient macros for common test patterns

## 🏗️ Architecture

- Parse assertion macros
- Snapshot testing utilities
- Error validation helpers

## 🔗 Ecosystem Integration

Used by all `data.text` language parsers for unit tests, integration tests for LSP features, and CI pipelines.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-testing).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
