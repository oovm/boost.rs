# schema-input-von-parser

手写 VON 词法与语法分析 crate：把 VON 文本解析为 `schema-input-von-ast` 的值树，错误以结构化诊断（附字节跨度与来源）报告。

- 语法与值模型分离：本 crate 只负责从文本到 AST，不定义值语义。
- 不依赖 serde；序列化桥接归 AST crate 的 `serde` feature。
