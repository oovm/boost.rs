# 🚀 language-semantic-tokens

[![Crates.io](https://img.shields.io/crates/v/language-semantic-tokens.svg)](https://crates.io/crates/language-semantic-tokens)
[![Documentation](https://docs.rs/language-semantic-tokens/badge.svg)](https://docs.rs/language-semantic-tokens)

**Semantic Tokens for Languages** — LSP-compatible semantic syntax highlighting for precise code coloring.

## 🎯 Why language-semantic-tokens?

Semantic tokens go beyond syntax highlighting by understanding the meaning of identifiers — distinguishing between function calls, variable references, type names, and more.

## ✨ Key Features

- **🎨 Semantic Token Type** — LSP-compatible with delta encoding
- **📊 Provider Trait** — `SemanticTokensProvider` for language-specific highlighting
- **🔧 Line Map Integration** — Works with `boost-source::LineMap` for position conversion
- **⚡ Efficient Encoding** — Delta-based encoding minimizes data transfer
- **🔄 Serde Support** — Optional serialization for LSP integration

## 🏗️ Architecture

- `SemanticToken` — LSP-compatible token with delta encoding
- `SemanticTokensProvider<L>` — Trait for providing semantic tokens

### LSP Compatibility

Matches LSP's `SemanticTokens` specification with delta encoding, token types, and modifiers via legend indices.

## 🔗 Ecosystem Integration

Used by `language-server` for `textDocument/semanticTokens` support, IDE extensions for enhanced highlighting, and code analysis tools.

## 📖 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-semantic-tokens).

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
