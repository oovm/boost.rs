# Glsl

GLSL support for the `data.text` language framework.

## Quick Start

```rust
use data_text_glsl::parse_glsl;

let root = parse_glsl("void main() { }").expect("valid GLSL");
assert_eq!(root.items.len(), 1);
```
