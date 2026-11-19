# `atlas-wire`

Atlas 运行时装配：类型化注册、作用域、冻结验证与构造期字段注入

Atlas 运行时装配层：构造期字段注入的运行时合同。

本 crate 拥有类型化键、注册表、provider、解析上下文、组件构造 trait、作用域、装配验证
与错误，不依赖过程宏；`atlas-macro` 生成的代码只调用这里的公开合同。

# 语义

`#[wire]` 字段由**环境**在构造期间提供，调用方只提交普通输入。构造成功才返回完整对象，
依赖解析失败直接返回 [`WireError`]，不使用未初始化内存、占位 `Default`、先填 `None`
后补值或构造后 setter。

```no_run
use std::sync::Arc;

use atlas_wire::{Component, Dependency, Environment, Resolver, Scope, WireError};

trait Repository: Send + Sync {
    fn name(&self) -> &str;
}

struct Orders;
impl Repository for Orders {
    fn name(&self) -> &str {
        "orders"
    }
}

struct Service {
    region: String,
    repository: Arc<dyn Repository>,
}

// 宏会生成同样的代码；这里手写以证明运行时本身不依赖宏。
impl Component for Service {
    type Input = String;

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<dyn Repository>>("repository", false)]
    }

    fn construct(region: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { region, repository: resolver.resolve::<Arc<dyn Repository>>(None)? })
    }
}

let mut builder = Environment::builder();
builder.register::<Arc<dyn Repository>, _>(
    Scope::Application,
    "repository",
    Vec::new(),
    |_| Ok(Arc::new(Orders) as Arc<dyn Repository>),
)?;
let environment = builder.freeze()?;
let service = environment.construct::<Service>("ap-guangzhou".into())?;
# Ok::<(), WireError>(())
```

# 作用域

[`Scope::Application`] 每个环境一份，[`Scope::Request`] 每个请求作用域一份，
[`Scope::Transient`] 每次解析一份。应用级 provider 不可直接或经 transient 间接依赖
请求级键；`freeze` 会拒绝这种图。
