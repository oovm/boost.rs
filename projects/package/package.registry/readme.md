# `package-registry`

包注册表渠道合同与通用适配器：包身份与版本元数据、制品位置、发布与令牌校验

包注册表渠道合同与通用适配器（`package.registry`）。

# 归属

本 crate 拥有第三方包渠道的通用机制：包身份与版本元数据、制品位置、发布请求与令牌校验，
以及 npm / JSR / NuGet / Maven / Conda 这几类真实渠道的请求规则。

不归本 crate 的内容：

- 版本约束选择、lock 生成与依赖图求解（`package.resolve`）。
- 下载落盘缓存与离线状态（`package.cache`）。
- 发布资格、发布计划与提交语义（`package.publish`）。
- 第三方渠道凭证的发现与写回。凭证属于宿主策略，必须由调用方显式注入
  [`types::PublishOptions::auth_token`]；本 crate 不扫描 `~/.npmrc`、Deno 配置或任何用户秘密。
- 自有渠道（产品自己的注册表服务）的适配器与签名格式，由产品侧持有。

# 传输

HTTP 走 [`boost_net`] 的同步 HTTP/1.1 传输。代理、DNS 与连接池策略不属于协议层，
需要这些策略的宿主可以用 [`HttpClient::with_transport`] 注入自己的 [`Transport`]。
