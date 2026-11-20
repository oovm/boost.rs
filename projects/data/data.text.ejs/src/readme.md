# 🛠️ JavaScript Parser Developer Guide

Javascript support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-javascript`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-javascript = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern JavaScript with async functions and JSX:

```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ejs::{language::EjsLanguage, parser::EjsParser};

fn main() {
    // 1. Prepare source code
    let code = r#"
        import React from 'react';

        const Greet = async ({ name }) => {
            const message = await fetchMessage(name);
            return (
                <div className="greeting">
                    <h1>{message}</h1>
                </div>
            );
        };
    "#;
    let source = SourceText::new(code);

    // 2. Initialize parser
    let language = EjsLanguage::new();
    let parser = EjsParser::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<EjsLanguage>::new(32);
    let result = parser.parse(&source, &[], &mut session);

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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract JavaScript-specific constructs like arrow functions, JSX elements, async/await blocks, or ESM imports/exports.

### 2. Incremental Parsing
No need to re-parse the entire file when small changes occur:
```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ejs::{language::EjsLanguage, parser::EjsParser};

// 复用 `ParseSession` 携带旧树：第二次 parse 只重解析变化区域
let language = EjsLanguage::new();
let parser = EjsParser::new(&language);
let mut session = ParseSession::<EjsLanguage>::new(32);
{
    let source = SourceText::new("<%= a %>");
    let result = parser.parse(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("<%= b %>");
    let result = parser.parse(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. Diagnostics
`data-text-javascript` provides rich error contexts specifically tailored for JS developers, handling complex scenarios like missing semicolons (ASI awareness) or malformed JSX:
```rust
use data_text_core::{Parser, SourceText, parser::ParseSession};
use data_text_ejs::{language::EjsLanguage, parser::EjsParser};

let language = EjsLanguage::new();
let parser = EjsParser::new(&language);
let mut session = ParseSession::<EjsLanguage>::new(32);
let source = SourceText::new("<% if (x) { %>");
let result = parser.parse(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes JavaScript source text into a stream of tokens, including support for template literals, JSX tags, and automatic semicolon insertion (ASI) hints.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle JavaScript's expression precedence, complex async/await flow, and JSX integration.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance JS analysis tools, bundlers, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various ECMAScript versions and JSX edge cases.
