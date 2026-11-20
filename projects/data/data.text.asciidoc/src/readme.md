# data-text-asciidoc

High-performance incremental AsciiDoc parser for the `data.text` family.

## Features

- Incremental parsing of AsciiDoc documents
- Support for headings, blocks, lists, tables, code blocks, and more
- Extensible architecture for custom AsciiDoc extensions
- Integration with the `data.text` family for language services

## Usage

```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_asciidoc::{AsciidocLanguage, AsciidocParser};

let code = "= Heading\n\nParagraph with *emphasis* and **strong** text.";
let source = SourceText::new(code);
let language = AsciidocLanguage::default();
let parser = AsciidocParser::new(&language);

// 词法分析由解析器内部驱动，这里直接使用 Parser 入口
let mut session = ParseSession::<AsciidocLanguage>::new(32);
let result = parser.parse(&source, &[], &mut session);
let _ = result;
```

## Features Flags

- `serde`: Enables serialization/deserialization support
- `lsp`: Enables Language Server Protocol support
- `mcp`: Enables Multi-Compiler Protocol support

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
