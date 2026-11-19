# 🚀 language-folding

[![Crates.io](https://img.shields.io/crates/v/language-folding.svg)](https://crates.io/crates/language-folding)
[![Documentation](https://docs.rs/language-folding/badge.svg)](https://docs.rs/language-folding)

**Code Folding Support for Languages** — Identify collapsible regions in source code for IDE outline views and editor folding.

## 🎯 Why language-folding?

Code folding helps developers manage large files by collapsing sections of code. `language-folding` provides a trait-based interface for identifying foldable regions in any `data.text`-supported language.

## ✨ Key Features

- **📁 Folding Provider Trait** — `FoldingProvider` for language-specific folding logic
- **🎯 Folding Range Types** — Comments, imports, and custom regions
- **📍 Precise Ranges** — Byte-offset based for accurate folding
- **🔄 Serde Support** — Optional serialization for LSP integration

## 🏗️ Architecture

- `FoldingRange` — Represents a foldable region with optional kind
- `FoldingRangeKind` — Comment, Imports, or Region
- `FoldingProvider<L>` — Trait for providing folding ranges

## 🔗 Ecosystem Integration

Used by `language-server` for `textDocument/foldingRange` support, IDE extensions for code outline views, and code formatters.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-folding).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
