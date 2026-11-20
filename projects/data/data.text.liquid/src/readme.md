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
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_liquid::{LiquidBuilder, LiquidLanguage};

// 词法分析由解析器内部驱动，用户通常通过 Builder 入口使用
let language = LiquidLanguage::new();
let builder = LiquidBuilder::new(&language);
let mut session = ParseSession::<LiquidLanguage>::new(32);
let source = SourceText::new("let x = 1;");
let result = builder.build(&source, &[], &mut session);
let _ = result;
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