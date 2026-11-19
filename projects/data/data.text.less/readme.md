# 🚀 CSS Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-css.svg)](https://crates.io/crates/data-text-css)
[![Documentation](https://docs.rs/data-text-css/badge.svg)](https://docs.rs/data-text-css)

**Styling the Web with Speed and Precision** — A high-performance, incremental CSS parser built on the `data.text` framework. Optimized for modern CSS features, large-scale stylesheets, and real-time developer tools.

## 🎯 Project Vision

CSS is the design language of the web, and its complexity has grown significantly with the introduction of variables, nesting, and complex layout modules. `data-text-css` aims to provide a robust, modern, Rust-powered infrastructure for parsing CSS that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive CSS files in real-time. Whether you are building custom linters, automated theme generators, or sophisticated IDE extensions, `data-text-css` provides the high-fidelity AST and efficiency needed to keep pace with the modern web design ecosystem.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large web projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only modified sections of large CSS files. Ideal for large-scale projects where maintainability and tool responsiveness are critical.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of modern CSS:
    - **Selectors**: Full support for complex selectors, including pseudo-classes, pseudo-elements, and attribute selectors.
    - **Modern Features**: Robust parsing of Media Queries, CSS Variables (Custom Properties), and the new CSS Nesting module.
    - **Layout Modules**: Precise mapping of Flexbox, Grid, and other modern layout properties.
    - **Functions & Units**: Detailed handling of CSS functions like `calc()`, `clamp()`, and various units (`rem`, `em`, `vh`, `vw`, etc.).
    - **At-Rules**: Comprehensive support for `@import`, `@media`, `@keyframes`, and other standard at-rules.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active styling.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent design token discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of CSS files.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
