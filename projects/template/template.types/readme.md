# template-types

模板族的底层共享类型：值、错误与 i18n。`template-types` 不依赖任何其它 `template.*`
crate，`template` 合同、输入引擎与 framework 都在它之上构建。

## 模块

| 模块 | 内容 |
|------|------|
| `values` | `Value` / `Context` / `IntoValue` 与过滤函数 |
| `types` | 类型标注、类型信息与 Rust 值集成 |
| `errors` | `DejavuError` / `DejavuResult` / `CompileError` / `RuntimeError` |
| `i18n` | `TranslationProvider` / `TranslationData` / `SimpleTranslator` |

## 边界

- 只提供跨引擎复用的数据类型与错误，不包含解析、IR、渲染或 AOT 编译。
- Dejavu 的语法与 IR 在 `template-input-dejavu`，共享合同在 `template`。
