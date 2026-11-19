# `atlas-macro`

Atlas 编译期生成层：`Component` derive 与 `#[wire]` 字段装配代码

Atlas 编译期生成层。

只提供一个入口 [`Component`]：识别字段上的 `#[wire]`，生成只含普通字段的输入类型、
依赖描述与 `atlas-wire` 的构造实现。宏只生成对运行时合同的调用，不把容器算法藏进展开
代码，也不对运行时 crate 建立普通依赖。

```ignore
use std::sync::Arc;

struct Primary;

#[derive(atlas::Component)]
#[atlas(input = OrderServiceInput, runtime = atlas::wire)]
struct OrderService {
    region: String,
    #[wire]
    repository: Arc<dyn OrderRepository + Send + Sync>,
    #[wire(key = Primary)]
    model: Arc<ModelClient>,
}
```
