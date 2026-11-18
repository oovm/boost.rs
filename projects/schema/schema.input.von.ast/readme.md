# schema-input-von-ast

语法无关的 VON 值模型：scalar、array、object 与 document 的树形表示，附带字段 / 条目路径（`MemberPath`）与来源映射，供 parser、门面与下游 schema 语义层共用。

- 只做数据建模，不做词法 / 语法分析——解析归 `schema-input-von-parser`。
- `serde` feature 提供值模型的序列化桥接，默认关闭。
