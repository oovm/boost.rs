# `language-highlight`

Cross-language highlight service: provider dispatch and host result adaptation

跨语言语法高亮服务。

# 归属

本 crate 拥有宿主 dispatch 与结果适配；词法/语法分类消费 `data.text` 与具体语言事实，语义高亮消费
具体语言的编译分析。颜色主题、ANSI、HTML 与编辑器 token scope 属于展示适配。

与 `language.format` 一样，高亮一个文档不需要启动 language server。
