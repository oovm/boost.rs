# Stylus

Stylus language support for the `data.text` framework.

## Features

- Lexical analysis for Stylus syntax
- Syntax parsing for Stylus constructs
- Abstract syntax tree (AST) representation
- Integration with `data.text` core framework

## Usage

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_stylus::{StylusBuilder, StylusLanguage};

let language = StylusLanguage::new();
let builder = StylusBuilder::new(&language);
let mut session = ParseSession::<StylusLanguage>::new(32);
let source = SourceText::new("body { color: red; }");
let result = builder.build(&source, &[], &mut session);
let _ = result;
```

## Implementation Status

- [x] Language trait implementation
- [x] Token types and roles
- [x] Element types and roles
- [x] Lexer implementation (basic)
- [x] Parser implementation (basic)
- [x] AST structure
- [x] Builder implementation (basic)
- [ ] Full lexer implementation
- [ ] Full parser implementation
- [ ] LSP support
- [ ] MCP support

## Contributing

Contributions are welcome! Please feel free to submit a pull request or open an issue.
