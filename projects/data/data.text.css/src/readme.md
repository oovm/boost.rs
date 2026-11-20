# 🛠️ CSS Parser Developer Guide

Css support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-css`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-css = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing a modern CSS file with variables, nesting, and media queries:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_css::{CssBuilder, CssLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        :root {
            --primary-color: #3498db;
        }

        .container {
            display: grid;
            grid-template-columns: repeat(3, 1fr);
            gap: 20px;

            & .item {
                background-color: var(--primary-color);
                padding: 1rem;
            }
        }

        @media (max-width: 768px) {
            .container {
                grid-template-columns: 1fr;
            }
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = CssLanguage::new();
    let builder = CssBuilder::new(language);

    // 3. Execute parsing
    let mut session = ParseSession::<CssLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract CSS-specific constructs like rule sets, selectors, declarations, media queries, or custom property definitions.

### 2. Incremental Parsing
No need to re-parse a massive CSS file when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_css::{CssBuilder, CssLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = CssLanguage::new();
let builder = CssBuilder::new(language);
let mut session = ParseSession::<CssLanguage>::new(32);
{
    let source = SourceText::new("body { color: red; }");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("body { color: blue; }");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. Diagnostics
`data-text-css` provides rich error contexts specifically tailored for designers and developers, handling complex scenarios like malformed selectors or invalid property values:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_css::{CssBuilder, CssLanguage};

let language = CssLanguage::new();
let builder = CssBuilder::new(language);
let mut session = ParseSession::<CssLanguage>::new(32);
let source = SourceText::new("div { color: ");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes CSS source text into a stream of tokens, including support for identifiers, strings, numbers with units, and special characters used in selectors.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle CSS's rule-based structure, complex selector precedence, and nested rules.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance CSS analysis tools, post-processors, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various CSS3/CSS4 edge cases and browser-specific hacks.
