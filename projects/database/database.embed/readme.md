# database-embed

嵌入式数据库门面：把 `database-*` 各层的存储、执行、查询与格式能力组装成单进程内嵌的数据库实例，供宿主程序直接 `use`，不经过任何线协议。

## 能力边界

- 只组装家族内部 crate（`database-storage` `database-execution` `database-query` `database-formats` 等），不依赖任何产品 crate，也不反向依赖 `database` 门面。
- 不解析线协议、不提供网络服务；远程访问归 `database-client` 与协议 crate。
- `sqlite` feature 接入下游 SQLite 产品 crate，仅用于端到端验收，默认关闭。

## Feature

| Feature | 默认 | 说明 |
|---------|------|------|
| `storage` | ✓ | 存储层能力 |
| `execution` | ✓ | 执行层能力 |
| `query` | ✓ | 查询层能力 |
| `formats` | ✓ | 格式层能力 |
| `default` | ✓ | `storage` + `execution` + `query` + `formats` |
| `sqlite` | — | 下游 SQLite 产品接入（验收专用） |
