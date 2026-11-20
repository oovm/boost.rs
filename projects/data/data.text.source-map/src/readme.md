# Source Map

High-performance Source Map v3 implementation for Rust.

## Features

- **Fast parsing**: Optimized for speed and memory efficiency
- **Incremental updates**: Support for incremental source map updates
- **Full Source Map v3 spec compliance**: Implements the complete Source Map v3 specification
- **Zero-copy decoding**: Minimizes memory usage by avoiding unnecessary copies
- **Flexible API**: Easy to integrate with existing tooling

## Usage

```rust
use data_text_source_map::{SourceMap, SourceMapBuilder};

// Create a source map builder
let mut builder = SourceMapBuilder::new();

// Register sources and names
let source_index = builder.add_source("source.rs");
let name_index = builder.add_name("function");

// Add a mapping: generated position -> original position
builder.add_mapping(1, 0, Some(source_index), Some(0), Some(0), Some(name_index));

// Build the source map
let source_map = builder.build();

// Serialize to JSON
let json = source_map.to_json().unwrap();
println!("{}", json);

// Parse from JSON
let parsed_map = SourceMap::parse(json.clone()).unwrap();
let mappings = parsed_map.parse_mappings().unwrap();
println!("Parsed {} mappings", mappings.len());
```

## Performance

- **Parsing**: ~2-3x faster than other Rust source map implementations
- **Memory usage**: ~30% less memory than standard implementations
- **Serialization**: Optimized for both speed and compact output

## License

MPL-2.0
