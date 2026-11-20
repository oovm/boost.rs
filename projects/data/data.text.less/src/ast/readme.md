# Less AST Module

This module defines the Abstract Syntax Tree (AST) structure for Less code.

## AST Nodes

- **LessRoot**: The root node of the Less AST, containing a list of top-level nodes.
- **LessNode**: A variant representing different types of Less nodes (rule sets, at-rules, comments).
- **RuleSet**: A Less rule set consisting of selectors and declarations.
- **Declaration**: A Less declaration consisting of a property name and value.
- **Selector**: A variant representing different types of Less selectors.
- **Value**: A variant representing different types of Less values.
- **AtRule**: A variant representing different types of Less at-rules.

## Usage

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_less::{LessBuilder, LessLanguage};

let language = LessLanguage::new();
let builder = LessBuilder::new(language);
let mut session = ParseSession::<LessLanguage>::new(32);
let source = SourceText::new("div { color: red; }");
let result = builder.build(&source, &[], &mut session);
let _ = result;
```

## Features

- **Serde Support**: AST nodes can be serialized and deserialized when the `serde` feature is enabled.
- **Source Location**: All AST nodes include span information for source location tracking.
- **Comprehensive Structure**: Covers all major Less constructs including selectors, rules, declarations, and values.