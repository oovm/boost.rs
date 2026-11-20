# CSS Builder Module

This module provides a builder for constructing CSS Abstract Syntax Trees (ASTs) from parsed green trees.

## Builder Functionality

The `CssBuilder` struct implements the `Builder` trait from `data_text_core` and provides the following functionality:

- **AST Construction**: Builds ASTs from parsed green trees
- **Error Handling**: Propagates parsing errors and diagnostics
- **Source Text Handling**: Processes source text and edits

## Usage

```rust
use data_text_core::{Builder, SourceText, parser::ParseSession};
use data_text_css::{CssBuilder, CssLanguage};

// Create a new builder with default configuration
let builder = CssBuilder::new(CssLanguage::default());

// Build AST from source text
let source = SourceText::new("div { color: red; }");
let mut session = ParseSession::<CssLanguage>::new(32);

let result = builder.build(&source, &[], &mut session);

match result.result {
    Ok(ast) => println!("AST built successfully: {ast:?}"),
    Err(error) => println!("Error building AST: {error:?}"),
}
```

## Features

- **Builder Trait Implementation**: Follows the standard builder pattern
- **Integration with Parser**: Works seamlessly with the CSS parser
- **Cache Support**: Uses builder cache for improved performance
- **Diagnostics**: Propagates parsing diagnostics to the caller