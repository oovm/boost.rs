# `language-format`

Cross-language formatting service: provider dispatch, document and range requests, configuration and versioned edits

跨语言格式化服务。

# 归属

本 crate 拥有**宿主 dispatch**：provider 注册、文档与范围请求、配置适配、版本化 edits 与结果交付。
具体格式化算法（CST 保真、printer 布局代数、语法规则）仍归 `data.text` 的语言 crate 或具体语言实现；
这里只做适配与编排，不复制布局引擎，也不从 AST 重印冒充 CST。

格式化一个文件不需要启动 language server：CLI、LSP、MCP 都消费本服务。
