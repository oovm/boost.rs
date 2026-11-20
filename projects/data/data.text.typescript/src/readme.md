# 🛠️ TypeScript Parser Developer Guide

Typescript support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-typescript`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-typescript = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern TypeScript with interfaces, generics, and TSX:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_typescript::{TypeScriptBuilder, TypeScriptLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        interface User<T> {
            id: T;
            name: string;
            greet(): void;
        }

        const Greet = ({ name }: User<number>) => {
            return (
                <div className="welcome">
                    <h1>Hello, {name}!</h1>
                </div>
            );
        };
    "#;
    let source = SourceText::new(code);

    // 2. Initialize parser
    let language = TypeScriptLanguage::standard();
    let builder = TypeScriptBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<TypeScriptLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract TypeScript-specific constructs like interface declarations, type aliases, generic parameters, or TSX elements.

### 2. Incremental Parsing
No need to re-parse the entire file when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_typescript::{TypeScriptBuilder, TypeScriptLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = TypeScriptLanguage::standard();
let builder = TypeScriptBuilder::new(&language);
let mut session = ParseSession::<TypeScriptLanguage>::new(32);
let old_result = builder.build(&SourceText::new("const a = 1;"), &[], &mut session);
let new_result = builder.build(&SourceText::new("const a = 2;"), &[], &mut session);
let _ = (old_result, new_result);
```

### 3. Diagnostics
`data-text-typescript` provides rich error contexts specifically tailored for TS developers, handling complex scenarios like missing type annotations or malformed TSX:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_typescript::{TypeScriptBuilder, TypeScriptLanguage};

let language = TypeScriptLanguage::standard();
let builder = TypeScriptBuilder::new(&language);
let mut session = ParseSession::<TypeScriptLanguage>::new(32);
let result = builder.build(&SourceText::new("const x: = ;"), &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes TypeScript source text into a stream of tokens, including support for type-only tokens, decorators, and TSX-specific tags.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle TypeScript's complex type-level syntax, expression precedence, and TSX integration.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance TS analysis tools, bundlers, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various TypeScript versions and TSX edge cases.
