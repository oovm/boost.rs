# `yydb`

YYDB 嵌入式数据库产品门面：按 `database` 门面的同一命名再导出能力，并暴露远程客户端。

下游 `yydb.rs` 只保留 N-API / PyO3 / WASM 绑定与宿主类型映射。
