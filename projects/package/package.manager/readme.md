# `package-manager`

包管理器编排：安装 / 更新 / 发布 / 登录与注册表来源配置

包管理器编排（`package.manager`）。

把清单 / lock（`package.resolve`）、渠道（`package.registry`）、缓存（`package.cache`）
与打包发布（`package.publish`）组合成安装、更新、发布、登录与审计流程。

宿主注入两件事：注册表适配器工厂（自有渠道与代理在这里接入）与凭据策略
（官方 CLI 存储的发现与写回不在包机制内）。
