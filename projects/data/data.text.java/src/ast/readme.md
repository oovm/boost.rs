# Java Abstract Syntax Tree (AST) Module

Java AST definitions

This module defines the Abstract Syntax Tree (AST) structure for the [Java programming language](https://www.oracle.com/java/). It provides a strongly-typed and comprehensive representation of Java source code, strictly adhering to the Java Language Specification (JLS).

## Purpose

The Java AST is the central data structure used by the compiler, static analysis tools, and IDE features to represent the semantic and structural information of a Java program. It captures the full range of Java constructs, from package declarations to complex expression trees.

## AST Node Types

### Core Structure
- **`JavaRoot`**: The root node representing a complete Java compilation unit.
- **`PackageDeclaration`**: Represents the `package` statement.
- **`ImportDeclaration`**: Represents `import` and `import static` statements.

### Type Declarations
- **`ClassDeclaration`**: Class definition including modifiers, type parameters, superclass, interfaces, and body.
- **`InterfaceDeclaration`**: Interface definition.
- **`EnumDeclaration`**: Enum definition with its constants and members.
- **`RecordDeclaration`**: Java 14+ record definition.
- **`AnnotationDeclaration`**: Annotation type definition.

### Members and Elements
- **`MethodDeclaration`**: Method definition with its signature and body.
- **`FieldDeclaration`**: Field definition with modifiers and initializers.
- **`ConstructorDeclaration`**: Class constructor definition.
- **`EnumConstant`**: Individual constant in an enum.
- **`Annotation`**: Represents an annotation usage (e.g., `@Override`).

### Statements and Expressions
- **`Statement`**: Represents various Java statements (local variables, blocks, control flow, etc.).
- **`Expression`**: The base type for all Java expressions (literals, assignments, calls, lambda expressions, etc.).
- **`Block`**: A sequence of statements enclosed in braces.
- **`TryStatement`**: Try-catch-finally construct, including try-with-resources.

### Types and Parameters
- **`Type`**: Representation of Java types (primitive, reference, array, wildcard, etc.).
- **`Parameter`**: Method or constructor parameter.
- **`TypeParameter`**: Generic type parameter definition.

## Usage Example

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_java::{JavaBuilder, JavaLanguage};

let language = JavaLanguage::new();
let builder = JavaBuilder::new(&language);
let mut session = ParseSession::<JavaLanguage>::new(32);
let source = SourceText::new("class Hello { }");
let result = builder.build(&source, &[], &mut session);
let _ = result;
```

## Design Principles

1. **JLS Fidelity**: Accurately reflects the structural rules of the Java Language Specification.
2. **Type Safety**: Uses Rust's rich type system to ensure AST validity and integrity.
3. **Rich Metadata**: Each node includes span information for precise source mapping.
4. **Extensibility**: Designed to be easily extended with semantic information during compilation phases.
