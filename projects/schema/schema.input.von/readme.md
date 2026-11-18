# schema-input-von

VON（Virtual Object Notation）值 / 文档输入门面：把 parser、AST 与值模型组装成一条从文本到 `VonValue` 的完整输入链，并统一 re-export 给下游。

## 组成

- `schema-input-von-parser`：手写词法与语法分析，产出 AST 与结构化诊断。
- `schema-input-von-ast`：语法无关的值模型（scalar / array / object / document）。

## Feature

| Feature | 默认 | 说明 |
|---------|------|------|
| `serde` | — | 值模型的 serde 桥接（透传 `schema-input-von-ast/serde`） |

门面只做组装与 re-export，不另设第二套语法或值语义。
