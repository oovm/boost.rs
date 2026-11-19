# `package-resolve`

包清单、工作区、版本约束、lock 文件与依赖求解的通用机制

包清单、工作区、版本约束、lock 与依赖求解（`package.resolve`）。

# 归属

本 crate 拥有依赖求解的通用机制：读入包/工作区清单，选择版本与来源，生成 lock，
以及工程磁盘布局的文件名约定。

不归本 crate 的内容：

- 渠道请求、制品下载与发布（`package.registry` / `package.publish`）。
- 下载落盘缓存与离线状态（`package.cache`）。
- 语言模块解析、import 绑定与链接器符号绑定。版本求解只产出锁定快照，编译器消费该快照，
  不能在名称解析失败时回头联网装包。
- 具体产品的清单文件名、用户数据目录与渠道 id；这些由调用方通过 [`ProjectLayout`]
  与 [`manifest::is_registry_publish_type_any`] 注入。
