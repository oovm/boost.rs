# 🌐 F# Language Integration

This module defines the `FSharpLanguage` struct, which serves as the central configuration point for the F# parser within the `data.text` framework.

## 🛠️ Language Configuration

The `FSharpLanguage` struct implements the `Language` trait and provides granular control over F# language features and versions.

### Version Support

You can configure the parser for specific F# versions:

```rust
use data_text_fsharp::FSharpLanguage;

let config = FSharpLanguage::new()
    .with_version(6, 0); // Enable F# 6.0 features
```

### Feature Toggles

Fine-tune specific language features:

```rust
use data_text_fsharp::FSharpLanguage;

let config = FSharpLanguage::new()
    .with_computation_expressions(true)
    .with_type_providers(false)
    .with_async_workflows(true);
```

## 🧩 Integration

`FSharpLanguage` links together the lexer and parser components:

- **TokenType**: `FSharpTokenType`
- **ElementType**: `FSharpElementType`
- **TypedRoot**: `()` (Future work will include typed root support)

## 🚀 Usage

```rust
use data_text_fsharp::{FSharpParser, FSharpLanguage};

let config = FSharpLanguage::new().with_all_features();
let parser = FSharpParser::new(&config);
```
