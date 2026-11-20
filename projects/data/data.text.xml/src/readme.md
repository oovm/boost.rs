# 🛠️ XML Parser Developer Guide

Xml support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-xml`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-xml = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing an XML document:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_xml::{builder::XmlBuilder, XmlLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <note>
            <to>Tove</to>
            <from>Jani</from>
            <heading>Reminder</heading>
            <body>Don't forget me this weekend!</body>
        </note>
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = XmlLanguage::new();
    let builder = XmlBuilder::new();

    // 3. Execute parsing
    let mut session = ParseSession::<XmlLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract XML elements, attributes, text content, and handle namespaces.

### 2. Incremental Parsing
No need to re-parse massive XML data exports or documents when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_xml::{builder::XmlBuilder, XmlLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = XmlLanguage::new();
let builder = XmlBuilder::new();
let mut session = ParseSession::<XmlLanguage>::new(32);
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
`data-text-xml` provides precise error feedback for malformed XML, such as unclosed tags, attribute quoting issues, or namespace prefix errors:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_xml::{builder::XmlBuilder, XmlLanguage};

let language = XmlLanguage::new();
let builder = XmlBuilder::new();
let mut session = ParseSession::<XmlLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes XML source text into a stream of tokens, handling tag boundaries, attribute names/values, character data, and processing instructions.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle XML's hierarchical structure and various node types.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance XML analysis, formatting, and validation tools.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various XML standards and edge cases.
