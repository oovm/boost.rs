# 🚀 data-text-vue

[![Crates.io](https://img.shields.io/crates/v/data-text-vue.svg)](https://crates.io/crates/data-text-vue)
[![Documentation](https://docs.rs/data-text-vue/badge.svg)](https://docs.rs/data-text-vue)

**Modern Web Development with Incremental Precision** — A high-performance, incremental Vue.js parser built on the `data.text` framework. Optimized for Vue 3 Single File Components (SFCs), complex template expressions, and responsive developer tools for the Vue ecosystem.

## 🎯 Project Vision

Vue.js is a leading framework for building modern web interfaces, and its Single File Component (SFC) architecture requires a sophisticated parser that can handle HTML templates, JavaScript/TypeScript logic, and CSS styles in a single file. `data-text-vue` aims to provide a robust, modern, Rust-powered infrastructure for parsing Vue SFCs that is both accurate and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, static analyzers, and refactoring tools that can handle massive Vue projects in real-time. Whether you are building custom linters for Vue components, automated migration tools, or sophisticated IDE extensions for Volar-like experiences, `data-text-vue` provides the high-fidelity AST and efficiency needed to support the modern web developer.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis in large Vue projects.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only the specific section (template, script, or style) that changed. Ideal for real-time IDE feedback and large-scale component analysis.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise syntax tree capturing the full depth of Vue SFCs:
    - **SFC Structure**: Precise mapping of `<template>`, `<script>`, `<script setup>`, and `<style>` blocks.
    - **Template Directives**: Detailed tracking of `v-if`, `v-for`, `v-bind`, `v-on`, and custom directives.
    - **Expressions**: Robust parsing of mustache interpolations `{{ ... }}` and directive expressions.
    - **Scoped Styles**: Support for parsing and analyzing scoped CSS/Sass/Less blocks.
    - **Indentation & Formatting**: Precise capture of indentation and whitespace for faithful code refactoring.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience during active coding.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent component discovery and analysis.

## 🏗️ Architecture

`data-text-vue` follows the modern Green/Red Tree architecture (inspired by Roslyn):

- **Green Tree**: Immutable, lossless, and syntax-only tree. It captures the full fidelity of the source code, including trivia (comments, whitespace).
- **Red Tree**: A facade over the Green Tree that provides a convenient, type-safe API for tree traversal and analysis, including parent pointers and absolute offsets.

This design enables efficient incremental parsing and powerful refactoring capabilities.


## 🛠️ Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.
