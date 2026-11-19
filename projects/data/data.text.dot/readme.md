# 🚀 DOT Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-dot.svg)](https://crates.io/crates/data-text-dot)
[![Documentation](https://docs.rs/data-text-dot/badge.svg)](https://docs.rs/data-text-dot)

**Visualizing Complexity with Speed** — A high-performance, incremental DOT parser built on the `data.text` framework. Optimized for Graphviz integration, automated diagram generation, and modern IDE support for the DOT graph description language.

## 🎯 Project Vision

The DOT language is the industry standard for describing graphs and networks, widely used for everything from architectural diagrams to state machines. `data-text-dot` provides a modern, Rust-powered infrastructure for analyzing and manipulating graph structures with extreme efficiency. By utilizing incremental parsing capabilities, it enables the creation of highly responsive visual tools that can handle massive graphs with sub-millisecond updates. Whether you are building automated diagram generators, network analysis tools, or sophisticated graph editors, `data-text-dot` provides the robust, efficient foundation you need for high-fidelity graph extraction.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to parse complex graph descriptions with sub-millisecond latency.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only modified nodes, edges, or attributes. Ideal for real-time graph visualization and interactive editing.
- **🌳 High-Fidelity AST**: Generates a precise Abstract Syntax Tree capturing the full structure of DOT:
    - **Graphs & Subgraphs**: Comprehensive mapping of directed/undirected graphs and nested subgraphs.
    - **Nodes & Edges**: Detailed tracking of node definitions, edge connections, and complex port mappings.
    - **Attributes**: Robust handling of global and local attribute lists for styling and metadata.
    - **Comments**: Retains all trivia for faithful code formatting and documentation generation.
- **🛡️ Industrial-Grade Fault Tolerance**: Gracefully recovers from syntax errors, providing clear and actionable diagnostics—critical for maintaining a smooth developer experience during active graph design.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent graph discovery and structural analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.

## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
