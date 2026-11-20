# 🛠️ Objective-C Parser Developer Guide

Objective-c support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-objective-c`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-objective-c = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern Objective-C with properties, categories, and blocks:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_objective_c::{ObjectiveCBuilder, ObjectiveCLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        #import <Foundation/Foundation.h>

        @interface User : NSObject
        @property (nonatomic, copy) NSString *name;
        - (void)greetWithCompletion:(void (^)(NSString *))completion;
        @end

        @implementation User
        - (void)greetWithCompletion:(void (^)(NSString *))completion {
            NSLog(@"Hello, %@", self.name);
            if (completion) {
                completion(@"Greeting finished");
            }
        }
        @end
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = ObjectiveCLanguage::new();
    let builder = ObjectiveCBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<ObjectiveCLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Objective-C specific constructs like interface/implementation blocks, property declarations, method definitions, or block expressions.

### 2. Incremental Parsing
No need to re-parse a massive Objective-C file when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_objective_c::{ObjectiveCBuilder, ObjectiveCLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = ObjectiveCLanguage::new();
let builder = ObjectiveCBuilder::new(&language);
let mut session = ParseSession::<ObjectiveCLanguage>::new(32);
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
`data-text-objective-c` provides rich error contexts specifically tailored for Objective-C developers, handling complex scenarios like missing `@end` keywords, malformed message expressions, or invalid block syntax:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_objective_c::{ObjectiveCBuilder, ObjectiveCLanguage};

let language = ObjectiveCLanguage::new();
let builder = ObjectiveCBuilder::new(&language);
let mut session = ParseSession::<ObjectiveCLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Objective-C source text into a stream of tokens, including support for `@`-prefixed keywords, message selectors, and C-style literals.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle Objective-C's unique message passing syntax, block structures, and mixed C/C++ integration.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance Objective-C analysis tools, static analyzers, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various Objective-C standards and edge cases.
