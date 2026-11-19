# data-text-liquid

High-performance incremental Liquid2 parser for the `data.text` family with flexible configuration, supporting template rendering and syntax analysis.

## Features

- **Incremental Parsing**: Efficiently parses only changed parts of templates
- **Flexible Configuration**: Customizable syntax and behavior
- **Template Rendering Support**: Built-in support for Liquid2 template rendering
- **Syntax Analysis**: Detailed AST for advanced tooling
- **LSP Integration**: Supports language server protocol for IDE integration

## Usage

```rust
use data_text_liquid::{LiquidLexer, LiquidParser, LiquidBuilder};

let source = "Hello, {{ name }}!";
let lexer = LiquidLexer::new();
let tokens = lexer.tokenize(source);
let parser = LiquidParser::new();
let ast = parser.parse(tokens);
let builder = LiquidBuilder::new();
let root = builder.build(ast);
```

## Syntax Support

- **Variables**: `{{ variable }}`
- **Blocks**: `{% block name %}{% endblock %}`
- **Comments**: `{# comment #}`
- **Control Structures**: `{% if %}`, `{% for %}`, `{% while %}`
- **Macros**: `{% macro name() %}{% endmacro %}`
- **Includes**: `{% include "template.html" %}`
- **Extensions**: `{% extends "base.html" %}`

## License

This project is licensed under the MIT License - see the [LICENSE](https://github.com/oovm/boost.rs/blob/master/License.md) file for details.