# Template

模板族：**模板源 → 文本**。独立顶级命名空间 `template.*`，不在 `document` 门面下再导出。

## 包图

分层自下而上：`template-types` 是最底层，不依赖任何 `template.*`；`template` 只提供合同；
输入引擎与 framework 在合同之上。

| 目录 | Package | 职责 |
|------|---------|------|
| `template.types/` | `template-types` | 底层共享值 / 错误 / i18n 类型 |
| `template/` | `template` | 合同：loader / 预算 / 诊断 / `TemplateEngine` trait |
| `template.input.dejavu/` | `template-input-dejavu` | Dejavu 语法、IR、渲染、AOT codegen、运行时工具与配置 |
| `template.input.hbs/` | `template-input-hbs` | Handlebars 语法子集 |
| `template.input.jinja/` | `template-input-jinja` | Jinja2 语法子集 |
| `template.framework/` | `template-framework` | 构建框架合同（页面入口 / 输入引擎编排 / 错误通道） |
| `template.framework.doki/` | `template-framework-doki` | Doki 构建框架实现与配置 schema |

目录名点分层，package 名连字符。输入引擎依赖 `template` 与 `template-types`；`template` 不反向
依赖任何引擎，测试因此不会形成 crate 环。Dejavu 的 AOT codegen、配置加载与运行时工具都是
`template-input-dejavu` 内的模块，不再拆成并列 crate。

## 边界

- 只做模板源 → 文本，输出 `RenderOutput { text, dependencies }`；不产出 `DocumentGraph`，不定义 `Block` / `Inline`。
- 渲染结果由调用方按 `capability().content` 交给既有输入层（HTML 用 `document.formats::import::html`），再走 `document-convert` 的现有路由。
- `template` 不进入 `document_formats::capabilities()`，也不出现在任何转换路由里。
- framework 只决定页面数据、入口模板与宿主扩展（如 i18n registry），不定义语法；渲染必须委托给输入引擎。
- 诊断码：合同/loader/预算为 `template.<failure>`，输入引擎为 `template.input.<engine>.<failure>`，framework 为 `template.framework.<name>.<failure>`。dejavu 引擎保留既有 `dejavu.template.*` 冻结码。
- 引擎统一 fail-closed：不支持的构造拒绝整次渲染并给稳定码，不静默丢弃、不改写成「看起来差不多」的文本。

设计与验收合同见文档仓 `规划设计/document/09-模板引擎命名空间与合同.md`。
