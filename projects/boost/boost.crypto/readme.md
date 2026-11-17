# `boost-crypto`

Boost 家族的共享密码基础能力：hash、HMAC/HKDF、AEAD、签名/验签、密钥交换数学、编码与秘密材料生命周期。本 crate 对审计过的纯 Rust 原语做薄门面，不承载任何协议逻辑（TLS 握手、密钥调度语义等归上层消费者）；首个完整消费者是 `boost-net` 的 TLS 1.3 实现。

安全熵一律复用 `boost-random` 的宿主安全熵合同（`Secure<SystemEntropy>`）：密钥生成与 RSA keygen 不接受固定 seed、不直接触碰平台随机接口。秘密材料生命周期由 [`secret::SecretBytes`] 承载：`Zeroize` + `ZeroizeOnDrop`、`Debug` 打码、无自由 `Deref`/`AsRef`，读取明文必须显式调用 `expose()`。

## 模块面

| 模块 | 能力 |
|------|------|
| `hash` | SHA-256 / SHA-384 / SHA-512 一次性摘要 |
| `mac` | HMAC-SHA256（含恒定时间验证）、HKDF-Extract / HKDF-Expand（raw `info`，TLS 1.3 Expand-Label 由调用方在外层组合） |
| `aead` | AES-128-GCM / AES-256-GCM seal/open，nonce 由调用方给出，库不生成 |
| `ecdsa` | P-256 / P-384 签名与验签，DER 与 fixed-width 两种编码透出并可互转 |
| `rsa` | PKCS#1 v1.5 与 PSS 的 SHA-256 / SHA-384 签名验签，keygen 用宿主熵 |
| `x25519` | 密钥生成（宿主熵）、公钥派生、DH 共享秘密（返回 `SecretBytes`） |
| `encoding` | base64 标准填充与无填充两种编解码 |
| `secret` | `SecretBytes` 秘密字节生命周期类型 |

## 吸收台账

按外部依赖吸收准入逐条登记。许可证以 registry 实际元数据为准。

| crate | 版本 | 许可证 | 用途 |
|-------|------|--------|------|
| `sha2` | 0.11.1 | MIT OR Apache-2.0 | SHA-256/384/512 摘要核心（`oid` feature 提供算法标识） |
| `hmac` | 0.13.0 | MIT OR Apache-2.0 | HMAC 构造与恒定时间验证 |
| `hkdf` | 0.13.0 | MIT OR Apache-2.0 | RFC 5869 HKDF-Extract/Expand |
| `aes-gcm` | 0.10.3 | Apache-2.0 OR MIT | AES-GCM AEAD（含 `aes`/`ghash`/`ctr` 传递实现） |
| `p256` | 0.13.2 | Apache-2.0 OR MIT | NIST P-256 ECDSA（`ecdsa` feature） |
| `p384` | 0.13.1 | Apache-2.0 OR MIT | NIST P-384 ECDSA（`ecdsa` feature） |
| `rsa` | 0.9.10 | MIT OR Apache-2.0 | RSA keygen、PKCS#1 v1.5 与 PSS 签名验签 |
| `signature` | 2.2.0 | Apache-2.0 OR MIT | `Signer`/`Verifier` 签名 trait 层（与 `p256`/`p384`/`rsa` 同版本） |
| `x25519-dalek` | 2.0.1 | BSD-3-Clause | X25519 DH 与 `StaticSecret`（`static_secrets` feature） |
| `base64` | 0.23.1 | MIT OR Apache-2.0 | base64 标准与无填充编解码 |
| `zeroize` | 1.9.1 | Apache-2.0 OR MIT | 秘密材料清零（`derive` feature，`Zeroizing`/`ZeroizeOnDrop`） |
| `sha2-rsa`（package `sha2`） | 0.10.9 | MIT OR Apache-2.0 | 仅为 `rsa` 0.9 提供 digest 0.10 世代的摘要类型（`oid` feature），别名先例见根清单 `sha1-rsa` |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | 把 `boost-random` 宿主熵适配到 `rsa`/`p256`/`p384`/`x25519-dalek` 的 RNG trait，本 crate 不直接请求平台随机 |

禁止项：`ring`、`aws-lc`、外部 OpenSSL/BoringSSL 及任何 C/汇编密码载荷。原语执行全部为纯 Rust。

## 验收

`tests/main.rs` 固定向量：SHA-256/384 NIST KAT、HMAC RFC 4231 case 1/2、HKDF RFC 5869 case 1、AES-128-GCM NIST CAVS（空 AAD 与带 AAD）、X25519 RFC 7748 §6.1；P-256/P-384 与 RSA-2048 为 keygen→sign→verify 往返加篡改拒绝；`SecretBytes` 验证 Debug 打码与 drop 清零。
