# `package-manager.legion`

Valkyrie 产品工具链：Legion 工程编排、Asgard 跨端交付，以及面向宿主的执行清单与 OVM 调用。

| 模块 | 职责 |
|------|------|
| `legion` | 工程发现、workspace 规划、编译/运行/测试/发布命令与构建缓存 |
| `asgard` | 跨端组件编译、静态站点与托管交付 |
| `product` | 宿主要用的结构化产品 API（plan / build / test / static build / serve） |
| `execution` | execution manifest 摘要与可信复用判定 |
| `ovm` | `.odin` 产物导出枚举与带 ABI 合同的调用 |

语言语义、HIR/MIR 与类型检查归 `language.valkyrie`；通用包管理机制归 `package.manager`。
本 crate 组合二者，不复制底层实现。
