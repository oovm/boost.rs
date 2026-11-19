# ActionScript MCP Server

This module implements a Model Context Protocol (MCP) server for ActionScript, allowing Large Language Models (LLMs) to interact with and understand ActionScript codebases.

## 🤖 What is MCP?

The Model Context Protocol (MCP) is an open protocol that enables seamless integration between AI models and local or remote data sources. By providing an MCP server, `data-text-actionscript` allows AI assistants to:
- Navigate ActionScript projects.
- Query AST structures.
- Retrieve precise diagnostic information.
- Understand code semantics for better generation and refactoring.

## 🚀 Usage

The server can be started via `serve_actionscript_mcp`, which listens for JSON-RPC commands over standard I/O (stdio). It leverages the `ActionScriptLanguageService` to provide deep language intelligence.
