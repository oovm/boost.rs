# 🛠️ R Parser Developer Guide

R support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-r`.

## 🚦 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing an R script:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_r::{RBuilder, RLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        # Calculate summary statistics
        calculate_stats <- function(data) {
          mean_val <- mean(data)
          sd_val <- sd(data)
          return(list(mean = mean_val, sd = sd_val))
        }

        dataset <- c(10, 20, 30, 40, 50)
        results <- calculate_stats(dataset)
        print(results)
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = RLanguage::new();
    let builder = RBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<RLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract R constructs like function definitions, vectorized operations, or formula notations.

### 2. Incremental Parsing
No need to re-parse the entire script when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_r::{RBuilder, RLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = RLanguage::new();
let builder = RBuilder::new(&language);
let mut session = ParseSession::<RLanguage>::new(32);
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
`data-text-r` provides rich error contexts specifically tailored for R developers, handling complex scenarios like unmatched brackets or malformed formula syntax:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_r::{RBuilder, RLanguage};

let language = RLanguage::new();
let builder = RBuilder::new(&language);
let mut session = ParseSession::<RLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes R source text into a stream of tokens, handling keywords, operators, literals, and R's unique identifier rules.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle R's expression precedence, lazy evaluation, and hierarchical structures.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance R analysis tools, IDEs, and data processing engines.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various R standards and edge cases.
