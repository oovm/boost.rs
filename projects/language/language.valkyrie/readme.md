# language.valkyrie

Valkyrie 语言的消费门面。宿主（Node-API、CLI、编辑器服务与测试 harness）只依赖本 crate，
不需要分别认识编译器、包管理与 OVM 执行三个底层能力。

## 归属

- 语言语义、类型模型、HIR/MIR、诊断与目标适配归 `compiler.valkyrie`，本 crate 只再导出。
- Legion 工程编排与 Asgard 交付归 `package.manager`，本 crate 只再导出。
- OVM 执行归 `interpreter.vm.ovm`，执行清单归 `package.manager::legion`。

## 表面

| 模块 | 作用 |
|------|------|
| `compiler` | 编译事实与稳定编译 API |
| `legion` | 工程规划、构建、测试与缓存 |
| `asgard` | 静态站点构建与托管 |
| `product` | 面向宿主的结构化产品 API（选项与结果均为纯 Rust 类型） |
| `execution` | execution manifest 摘要与可信复用判定 |
| `ovm` | `.odin` 导出枚举与带 ABI 合同的调用 |

`product` / `execution` / `ovm` 不包含任何宿主绑定类型；Node-API 绑定层负责把这些
纯 Rust 结构映射为宿主对象，不得在算法层引入宿主对象。
