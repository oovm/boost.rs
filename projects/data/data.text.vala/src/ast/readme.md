# Vala Abstract Syntax Tree (AST) Module

This module defines the Abstract Syntax Tree (AST) structure for the [Vala programming language](https://vala.dev/). It provides a strongly-typed and comprehensive representation of Vala source code, optimized for the GObject system and C code generation.

## Purpose

The Vala AST is the central data structure used by the compiler and associated tools to represent the semantic structure of a Vala program. It captures Vala's object-oriented features, its unique integration with GLib/GObject, and its support for modern programming constructs like async/await and signals.

## AST Node Types

### Core Structure
- **`ValaRoot`**: The root node representing a complete Vala source file or translation unit.
- **`Namespace`**: Represents a Vala namespace containing other declarations.
- **`UsingDirective`**: Represents a `using` statement.

### Declarations
- **`Class`**: Class definition with its base types, fields, methods, properties, and signals.
- **`Interface`**: Interface definition.
- **`Struct`**: Struct definition.
- **`Enum`**: Enum definition with its members.
- **`Delegate`**: Delegate type definition.
- **`Method`**: Method definition with parameters, return type, and body.
- **`Property`**: GObject property definition with `get` and `set` accessors.
- **`Signal`**: GObject signal definition.

### Expressions and Statements
- **`Expression`**: The base type for all expressions (literals, identifiers, calls, assignments, etc.).
- **`Statement`**: Represents various statements (local variables, control flow, blocks, etc.).
- **`Block`**: A sequence of statements enclosed in braces.
- **`TryStatement`**: Try-catch-finally construct.

### GObject Specifics
- **`Constructor`**: Represents `construct` blocks and class constructors.
- **`SignalConnection`**: Represents signal connection expressions (e.g., `obj.sig.connect(...)`).

## Usage Example

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_vala::{ValaBuilder, ValaLanguage};

let language = ValaLanguage::new();
let builder = ValaBuilder::new(&language);
let mut session = ParseSession::<ValaLanguage>::new(32);
let source = SourceText::new("class Hello : Object { }");
let result = builder.build(&source, &[], &mut session);
let _ = result;
```

## Design Principles

1. **GObject Fidelity**: Accurately represents Vala's specialized GObject features.
2. **Type Safety**: Leverages Rust's type system to ensure AST structural integrity.
3. **Rich Metadata**: Each node includes span information for precise source mapping.
4. **Efficiency**: Designed for fast traversal and transformation, suitable for large projects.
