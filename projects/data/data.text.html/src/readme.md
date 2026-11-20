# 🛠️ HTML Parser Developer Guide

Html support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-html`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-html = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a modern HTML5 document with attributes and nested elements:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_html::{HtmlBuilder, HtmlLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <title>HTML Example</title>
        </head>
        <body>
            <div id="app" class="container">
                <h1>Hello, Boost!</h1>
                <img src="logo.png" alt="Boost Logo" />
            </div>
            <script src="app.js"></script>
        </body>
        </html>
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = HtmlLanguage::new();
    let builder = HtmlBuilder::new(language);

    // 3. Execute parsing
    let mut session = ParseSession::<HtmlLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract HTML-specific constructs like element tags, attribute values, text content, or specific `script` and `style` blocks.

### 2. Incremental Parsing
No need to re-parse a massive HTML document when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_html::{HtmlBuilder, HtmlLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = HtmlLanguage::new();
let builder = HtmlBuilder::new(language);
let mut session = ParseSession::<HtmlLanguage>::new(32);
{
    let source = SourceText::new("a");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("b");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. Diagnostics
`data-text-html` provides rich error contexts specifically tailored for web developers, handling complex scenarios like unclosed tags or malformed attribute syntax:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_html::{HtmlBuilder, HtmlLanguage};

let language = HtmlLanguage::new();
let builder = HtmlBuilder::new(language);
let mut session = ParseSession::<HtmlLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes HTML source text into a stream of tokens, including support for tags, attributes, text nodes, and special handling for `script`/`style` content.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle HTML's hierarchical structure, void elements, and self-closing tags.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance HTML analysis tools, scrapers, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various HTML5 edge cases and "tag soup."
