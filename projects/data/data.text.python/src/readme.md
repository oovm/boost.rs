# 🛠️ Python Parser Developer Guide

Python support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-python`.

## 🚦 Quick Start

### Basic Parsing Example

The following is a standard workflow for parsing a Python function with type hints and decorators:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_python::{PythonBuilder, PythonLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        @dataclass
        class User:
            name: str
            age: int
            
            async fn greet(self) -> str:
                return f"Hello, {self.name}!"
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = PythonLanguage::new();
    let builder = PythonBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<PythonLanguage>::new(32);
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
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Python-specific constructs like class definitions, indentation-based blocks, type annotations, or decorators.

### 2. Incremental Parsing
No need to re-parse the entire module when small changes occur:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_python::{PythonBuilder, PythonLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = PythonLanguage::new();
let builder = PythonBuilder::new(&language);
let mut session = ParseSession::<PythonLanguage>::new(32);
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
`data-text-python` provides rich error contexts specifically tailored for Python developers, handling indentation errors and modern syntax requirements:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_python::{PythonBuilder, PythonLanguage};

let language = PythonLanguage::new();
let builder = PythonBuilder::new(&language);
let mut session = ParseSession::<PythonLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Python source text into a stream of tokens, including complex indentation/dedentation logic and support for f-strings.
- **Parser**: Syntax analyzer based on the Pratt parsing algorithm to handle Python's expression precedence, structural pattern matching, and block identification.
- **AST**: A strongly-typed syntax abstraction layer designed for high-performance Python analysis tools, linters, and IDEs.

## 🔗 Advanced Resources

- **Full Examples**: Check the [examples/](examples/) folder in the project root.
- **API Documentation**: Run `cargo doc --open` for detailed type definitions.
- **Test Cases**: See [tests/](tests/) for handling of various Python versions and edge cases.
