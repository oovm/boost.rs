# VOML Lexer Module

The VOML Lexer module provides lexical analysis for the VOML (V-Object-Markup-Language). It converts VOML source text into a stream of tokens, serving as the first phase of parsing and data extraction.

## Purpose

This module is designed to handle the specific lexical requirements of VOML, a language that combines structural object definitions with markup-like capabilities. It ensures that identifiers, literals, and keywords are correctly identified and located within the source text.

## Features

- **Structural Keyword Recognition**: Supports keywords for defining objects, modules, and relationships.
- **Flexible Identifier Parsing**: Handles names for objects, properties, and modules, including support for specialized naming conventions.
- **Comprehensive Literal Support**: Correctly tokenizes numeric (integer, float), string (single/double quotes), and boolean literals.
- **Comment Handling**: Supports both single-line (`//`) and multi-line (`/* ... */`) comments for documentation within VOML files.
- **Detailed Span Tracking**: Every token is associated with a precise span (start and end offsets), enabling accurate error diagnostics and source-aware tools.
- **Performance Optimized**: Built for speed to handle large VOML configuration or data files efficiently.

## Token Types

### Keywords
- **Core Structure**: `module`, `import`, `object`, `schema`.
- **Property Modifiers**: `pub`, `mut`, `const`, `required`, `optional`.
- **Value Types**: `int`, `string`, `bool`, `f32`, `f64`, `list`, `map`.
- **Control & Flow**: `if`, `else`, `for`, `match`.

### Literals
- **Numeric**: `42`, `3.14159`, `1e-10`.
- **String**: `"VOML Data"`, `'Literal String'`.
- **Boolean**: `true`, `false`.
- **Special**: `null`, `none`.

### Operators and Delimiters
- **Structural**: `{`, `}`, `[`, `]`, `(`, `)`.
- **Separators**: `,`, `:`, `;`.
- **Access**: `.`, `->`.
- **Assignment**: `=`, `:=`.

## Usage Example

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_voml::{VomlBuilder, VomlLanguage};

// 词法分析由解析器内部驱动，用户通常通过 Builder 入口使用
let language = VomlLanguage::new();
let builder = VomlBuilder::new(&language);
let mut session = ParseSession::<VomlLanguage>::new(32);
let source = SourceText::new("let x = 1;");
let result = builder.build(&source, &[], &mut session);
let _ = result;
```

## Error Handling

The lexer identifies and reports:
- **Lexical Errors**: Characters that do not belong to the VOML alphabet.
- **Unclosed Tokens**: Unterminated strings or comments.
- **Invalid Literals**: Malformed numbers or boolean values.
- **Precise Location**: All errors include position information for easy debugging.

## Design Principles

1. **Clarity**: The lexer is designed to be straightforward and easy to maintain.
2. **Standard Alignment**: Follows the official VOML specification for lexical structures.
3. **Efficiency**: Minimizes memory allocations during tokenization.
