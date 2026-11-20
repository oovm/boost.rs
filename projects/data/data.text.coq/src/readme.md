# 🛠️ Coq Parser Developer Guide

Coq support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-coq`.

## 🚦 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing a simple Coq theorem:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_coq::{CoqBuilder, CoqLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        Theorem add_0_n : forall n : nat, 0 + n = n.
        Proof.
          intros n. reflexivity.
        Qed.
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = CoqLanguage::new();
    let builder = CoqBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<CoqLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Coq constructs like theorem declarations, proof tactics, or Gallina terms.

### 2. Incremental Parsing
No need to re-parse the entire script when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_coq::{CoqBuilder, CoqLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = CoqLanguage::new();
let builder = CoqBuilder::new(&language);
let mut session = ParseSession::<CoqLanguage>::new(32);
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
`data-text-coq` provides rich error contexts specifically tailored for Coq developers:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_coq::{CoqBuilder, CoqLanguage};

let language = CoqLanguage::new();
let builder = CoqBuilder::new(&language);
let mut session = ParseSession::<CoqLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Coq source text into a stream of tokens, handling keywords, operators, identifiers, and complex notation delimiters.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle Coq's extensible syntax, vernacular commands, and tactic blocks.
- **AST**: A strongly-typed syntax abstraction layer designed for building high-performance Coq analysis tools and interactive proof environments.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various Coq notation edge cases and formal development structures.
