# 🚀 data-text-resolver

[![Crates.io](https://img.shields.io/crates/v/data-text-resolver.svg)](https://crates.io/crates/data-text-resolver)
[![Documentation](https://docs.rs/data-text-resolver/badge.svg)](https://docs.rs/data-text-resolver)

**Symbol Resolution for Languages** — Cross-file symbol resolution and import handling for language analysis.

## 🎯 Why data-text-resolver?

Modern codebases span multiple files and modules. `data-text-resolver` provides the infrastructure for resolving symbol references across file boundaries, handling imports, and building symbol indexes.

## ✨ Key Features

- **🔍 Cross-File Resolution** — Resolve symbols across multiple source files
- **📦 Import Handling** — Process import statements and module dependencies
- **📊 Symbol Indexing** — Build and query symbol indexes for fast lookups
- **🌐 Workspace Support** — Scale from single files to large projects
- **🔄 Incremental Updates** — Efficiently update resolution on file changes

## 🏗️ Architecture

Resolution process: Parse → Collect → Index → Resolve

## 🔗 Ecosystem Integration

Integrates with `boost-source` for file access, `language-navigation` for definition/reference providers, and `language-server` for workspace symbol support.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/data-text-resolver).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
