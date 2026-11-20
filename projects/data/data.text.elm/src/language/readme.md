# 🌐 Elm Language Integration

This module defines the `ElmLanguage` struct, which serves as the central configuration point for the Elm parser within the `data.text` framework.

## 🛠️ Language Configuration

The `ElmLanguage` struct implements the `Language` trait, providing metadata and type definitions required by the `data.text` family.

```rust
pub struct ElmLanguage {
    // Future configuration options (e.g., Elm version, compiler options)
}
```

## 🧩 Integration

`ElmLanguage` links together the lexer and parser components:

- **TokenType**: `ElmTokenType`
- **ElementType**: `ElmElementType`
- **TypedRoot**: `()` (Future work will include typed root support)

## 🚀 Usage

```rust
use data_text_elm::ElmLanguage;

let config = ElmLanguage::new();
// Pass this config to ElmParser::new(&config)
```
