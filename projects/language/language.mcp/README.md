# 馃殌 language-mcp

[![Crates.io](https://img.shields.io/crates/v/language-mcp.svg)](https://crates.io/crates/language-mcp)
[![Documentation](https://docs.rs/language-mcp/badge.svg)](https://docs.rs/language-mcp)

**Model Context Protocol Integration for `data.text`** 鈥?Enable AI assistants to understand and analyze code through the MCP standard.

## 馃幆 Why language-mcp?

The Model Context Protocol (MCP) is an open standard that enables AI assistants to interact with development tools and codebases. `language-mcp` bridges the parsing capabilities with MCP, allowing AI agents to perform code analysis and understanding tasks.

## 鉁?Key Features

- **馃 MCP Server Implementation** 鈥?Standard MCP server exposing `data.text` language analysis
- **馃搳 Code Analysis Tools** 鈥?Parsing, symbol extraction, and navigation as MCP tools
- **馃攳 Semantic Understanding** 鈥?Query symbol definitions, references, and documentation
- **馃搧 Project-Wide Analysis** 鈥?Multi-file project understanding via `boost-source`
- **馃寪 Language Agnostic** 鈥?Works with any `data.text` language parser

## 馃彈锔?Architecture

### MCP Tools Provided

| Tool | Description |
|------|-------------|
| `parse_file` | Parse source file and return AST structure |
| `find_symbols` | Search for symbols matching a query |
| `get_definition` | Get definition location of a symbol |
| `find_references` | Find all references to a symbol |
| `get_hover` | Get hover information for a position |

## 馃敆 Ecosystem Integration

Integrates with `data-text-core` for parsing, `boost-source` for file access, `language-navigation` for code navigation, and any MCP-compatible AI assistant.

## 馃摉 Documentation

For usage examples and API details, see the [API documentation](https://docs.rs/language-mcp).

## 馃 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
