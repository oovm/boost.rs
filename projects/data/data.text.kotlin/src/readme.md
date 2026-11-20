# 🛠️ Kotlin Parser Developer Guide

Kotlin support for the `data.text` language framework.

This guide is designed to help you quickly get started with developing and integrating `data-text-kotlin`.

## 🚦 Quick Start

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
data-text-kotlin = { path = "..." }
```

### Basic Parsing Example

The following is a standard workflow for parsing modern Kotlin with data classes, coroutines, and lambdas:

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

let language = KotlinLanguage::new();
let builder = KotlinBuilder::new(&language);
let mut session = ParseSession::<KotlinLanguage>::new(32);
let source = SourceText::new("x = = ;");
let result = builder.build(&source, &[], &mut session);
for diagnostic in result.diagnostics {
    println!("{diagnostic:?}");
}
```

## 🔍 Core API Usage

### 1. Syntax Tree Traversal
After a successful parse, you can use the built-in visitor pattern or manually traverse the Green/Red Tree to extract Kotlin specific constructs like data classes, sealed classes, suspend functions, lambdas, and property delegates.

### 2. Incremental Parsing
Kotlin projects (especially Android or KMP projects) can be massive. `data-text-kotlin` supports sub-millisecond incremental updates:
```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

// 复用 `ParseSession` 携带旧树：第二次 build 只重解析变化区域
let language = KotlinLanguage::new();
let builder = KotlinBuilder::new(&language);
let mut session = ParseSession::<KotlinLanguage>::new(32);
{
    let source = SourceText::new("val a = 1");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
{
    let source = SourceText::new("val a = 2");
    let result = builder.build(&source, &[], &mut session);
    println!("{}", result.diagnostics.len());
}
```

### 3. KMP and Multiplatform Support
The parser correctly handles Kotlin Multiplatform (KMP) specific syntax like `expect`/`actual` declarations, ensuring accurate trees across all targets.

## 📊 Code Analysis Examples

### 1. Analyzing Kotlin Data Classes

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        data class Person(
            val id: String,
            val name: String,
            val age: Int,
            val email: String?
        ) {
            fun fullName() = name
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = KotlinLanguage::new();
    let builder = KotlinBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<KotlinLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

### 2. Analyzing Coroutine Functions

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        class ApiService {
            private val client = HttpClient()
            
            suspend fun fetchData(url: String): Result<Data, ApiError> = withContext(Dispatchers.IO) {
                try {
                    val response = client.get(url)
                    val data = response.body<Data>()
                    Result.success(data)
                } catch (e: Exception) {
                    Result.failure(ApiError(e.message))
                }
            }
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = KotlinLanguage::new();
    let builder = KotlinBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<KotlinLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

### 3. Analyzing Extension Functions

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        fun String.capitalizeWords(): String {
            return split(" ")
                .map { it.capitalize() }
                .joinToString(" ")
        }
        
        fun List<Int>.sumEven(): Int {
            return filter { it % 2 == 0 }
                .sum()
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = KotlinLanguage::new();
    let builder = KotlinBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<KotlinLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

### 4. Analyzing Sealed Classes and When Expressions

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

fn main() {
    // 1. Prepare source code
    let code = r#"
        sealed class Result<T> {
            data class Success<T>(val data: T) : Result<T>()
            data class Error<T>(val message: String) : Result<T>()
        }
        
        fun <T> handleResult(result: Result<T>) {
            when (result) {
                is Result.Success -> println("Success: ${result.data}")
                is Result.Error -> println("Error: ${result.message}")
            }
        }
    "#;
    let source = SourceText::new(code);

    // 2. Initialize builder
    let language = KotlinLanguage::new();
    let builder = KotlinBuilder::new(&language);

    // 3. Execute parsing
    let mut session = ParseSession::<KotlinLanguage>::new(32);
    let result = builder.build(&source, &[], &mut session);

    // 4. Handle results
    if result.diagnostics.is_empty() {
        println!("Parsing successful!");
    } else {
        eprintln!("Errors found during parsing: {:#?}", result.diagnostics);
    }
}
```

## 🏗️ Architecture Overview

- **Lexer**: Tokenizes Kotlin source text, supporting complex string templates, backticked identifiers, and various numeric literal formats.
- **Parser**: A high-performance syntax analyzer that handles Kotlin's expressive grammar, including generics, coroutines, and modern language features.
- **AST**: A strongly-typed, lossless syntax tree that preserves all trivia (comments/whitespace) for refactoring and formatting tools.

## 🛠️ Performance & Reliability

- **High-Fidelity AST**: Retains all trivia (whitespace and comments), making it ideal for code formatting and refactoring tools.
- **Fault Tolerance**: Automatically recovers from syntax errors to provide as much information as possible from the rest of the file.
- **Memory Efficiency**: Leverages immutable data structures (Green Trees) for low-overhead tree management.
- **Incremental Updates**: Sub-millisecond parsing for large files with small changes, perfect for IDE integration.

## 📚 API Reference

### Core Components

- **KotlinParser**: The main parser implementation for Kotlin source code
- **Kotlin**: Language configuration and settings
- **KotlinLexer**: Tokenizer for Kotlin source text
- **KotlinToken**: Token types for Kotlin syntax
- **KotlinElement**: Element types for Kotlin AST nodes

### Common Use Cases

1. **IDE Integration**: Real-time syntax highlighting and error checking
2. **Static Analysis**: Code quality tools and linters
3. **Refactoring Tools**: Automated code transformations
4. **Documentation Generation**: Extracting code structure for documentation
5. **KMP Support**: Analyzing multiplatform codebases

## 🔧 Advanced Configuration

### Customizing Parser Behavior

```rust
use data_text_kotlin::{KotlinBuilder, KotlinLanguage};

fn configure_parser() {
    let language = KotlinLanguage::new();
    let builder = KotlinBuilder::new(&language);

    println!("Builder configured for Kotlin language");
}
```

## 🎯 Kotlin-Specific Features

The parser supports all major Kotlin features:

- **Data Classes**: Automatic generation of equals(), hashCode(), toString(), etc.
- **Sealed Classes**: Restricted class hierarchies for pattern matching
- **Coroutines**: Asynchronous programming with suspend functions
- **Extension Functions**: Adding functionality to existing types
- **Null Safety**: Safe handling of nullable types
- **Smart Casts**: Automatic type casting based on type checks
- **Lambda Expressions**: Concise function literals
- **Property Delegates**: Custom property behavior
- **Type Inference**: Automatic type deduction
- **Operator Overloading**: Custom operator implementations
