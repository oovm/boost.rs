# TypeScript LSP

This module provides TypeScript language services for the Boost `data.text` project:

- hover on declarations (function, class, interface, type alias, enum, namespace, variable, members, imports/exports);
- go-to-definition and find-references for module-level bindings, backed by `data_text_typescript::scope::module_scope_facts`;
- whole-document formatting through `language_typescript::formatter`, returned as a single edit.

Semantics live here; `mcp` only wraps this service.
