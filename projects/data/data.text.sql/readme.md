# 🚀 SQL Parser

[![Crates.io](https://img.shields.io/crates/v/data-text-sql.svg)](https://crates.io/crates/data-text-sql)
[![Documentation](https://docs.rs/data-text-sql/badge.svg)](https://docs.rs/data-text-sql)

**Structured Power and Speed for Data** — A high-performance, incremental SQL parser built on the `data.text` framework. Optimized for multi-dialect support (MySQL, PostgreSQL, SQLite), complex query analysis, and real-time database tooling.

## 🎯 Project Vision

SQL is the universal language of data, but its diverse dialects and complex syntax make it challenging to parse accurately and efficiently. `data-text-sql` aims to provide a robust, modern, Rust-powered infrastructure for parsing SQL that is both dialect-aware and incredibly fast. By utilizing incremental parsing architecture, we enable the creation of highly responsive IDEs, database managers, static analyzers, and migration tools that can handle massive SQL scripts and complex schemas in real-time. Whether you are building custom query optimizers, security auditors, or sophisticated database developer tools, `data-text-sql` provides the high-fidelity AST and efficiency needed to manage the complexities of modern data environments.

## ✨ Core Features

- **⚡ Blazing Fast**: Leverages Rust's performance and memory safety to provide sub-millisecond parsing, essential for high-frequency developer tools and real-time analysis of large SQL scripts.
- **🔄 Incremental by Nature**: Built-in support for partial updates—re-parse only modified sections of large SQL files. Ideal for database migration scripts and real-time schema editing.
- **🌳 High-Fidelity AST**: Generates a comprehensive and precise Abstract Syntax Tree capturing the full depth of SQL:
    - **Multi-Dialect Support**: Robust parsing for MySQL, PostgreSQL, SQLite, and ANSI SQL standards.
    - **DDL & DML**: Deep support for Data Definition Language (`CREATE`, `ALTER`, `DROP`) and Data Manipulation Language (`SELECT`, `INSERT`, `UPDATE`, `DELETE`).
    - **Complex Queries**: Precise mapping of Joins, Subqueries, CTEs (Common Table Expressions), and Window Functions.
    - **Stored Procedures & Triggers**: Support for parsing complex procedural logic across different dialects.
    - **Schema Awareness**: Detailed tracking of tables, columns, constraints, and relationships.
- **🛡️ Industrial-Grade Fault Tolerance**: Engineered to recover from syntax errors gracefully, providing precise diagnostics—crucial for maintaining a smooth developer experience when writing complex queries.
- **🧩 Deep Ecosystem Integration**: Seamlessly works with `language-server` for full LSP support and `language-mcp` for intelligent database schema discovery and analysis.

## 🏗️ Architecture

The parser follows the **Green/Red Tree** architecture (inspired by Roslyn), which allows for:
1. **Efficient Immutability**: Share nodes across different versions of the tree without copying.
2. **Lossless Syntax Trees**: Retains all trivia (whitespace and comments), enabling faithful code formatting and refactoring of SQL scripts.
3. **Type Safety**: Strongly-typed "Red" nodes provide a convenient and safe API for tree traversal and analysis.


## 🤝 Contributing

We welcome contributions of all kinds! If you find a bug, have a feature request, or want to contribute code, please check our [issues](https://github.com/oovm/boost.rs/issues) or submit a pull request.
