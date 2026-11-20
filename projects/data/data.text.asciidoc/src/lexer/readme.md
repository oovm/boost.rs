# 🔍 AsciiDoc Lexer

The AsciiDoc lexer is responsible for tokenizing AsciiDoc source code into meaningful tokens for further processing by the parser.

## ✨ Core Features

- **Comprehensive Tokenization**: Handles all AsciiDoc syntax elements including headings, lists, tables, and more.
- **Efficient Scanning**: Optimized for performance with minimal backtracking.
- **Error Recovery**: Gracefully handles malformed syntax and continues processing.
- **Configurable**: Supports customization through the AsciiDocLanguage configuration.

## 📋 Token Types

The lexer produces tokens for:
- Headings
- Lists
- Tables
- Code blocks
- Emphasis
- Images
- Attributes and macros
- Comments
- And many more AsciiDoc-specific elements

## 🔧 Usage

```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_asciidoc::{AsciidocLanguage, AsciidocParser};

// 词法分析由解析器内部驱动，用户通常通过 Parser 入口使用
let language = AsciidocLanguage::default();
let parser = AsciidocParser::new(&language);
let mut session = ParseSession::<AsciidocLanguage>::new(32);
let source = SourceText::new("= Hello World\n\nThis is AsciiDoc");
let result = parser.parse(&source, &[], &mut session);
let _ = result;
```