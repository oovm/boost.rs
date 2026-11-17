# `boost-crypto`

Boost 家族的共享密码基础能力：hash、HMAC/HKDF、AEAD、签名/验签、密钥交换数学、编码与秘密材料生命周期。不承载任何协议逻辑（TLS 握手、密钥调度语义等归上层消费者）；首个完整消费者是 `boost-net` 的 TLS 1.3 实现。

安全熵一律复用 `boost-random` 的宿主安全熵合同：本 crate 不实现外部 RNG trait，不直接请求平台随机接口，也不持有可重放的确定性流。每次取熵都可能失败，错误按[`CryptoError::Entropy`]逐次传回；不存在先探测再 `expect` 的路径，不会零填充、返回伪成功或回退普通 PRNG。秘密材料生命周期由 [`secret::SecretBytes`] 承载：`Zeroize` + `ZeroizeOnDrop`、`Debug` 打码、无自由 `Deref`/`AsRef`，读取明文必须显式调用 `expose()`。

## 当前状态（未签收）

本 crate 仍在继续原语内化，**不等于全部签收**。`rsa` 仍由外部实现闭包提供公钥解析、验签与 `BigUint` 模幂，并携带 `rand_core`/`rand` 传递依赖；`boost.random` 的 web 熵后端仍未移除。按现行内化合同，这只算部分吸收，不能记为已通过；剩余外部实现闭包是独立切片。

## 模块面

| 模块 | 能力 |
|------|------|
| `hash` | SHA-256 / SHA-384 / SHA-512 一次性摘要 |
| `mac` | HMAC-SHA256 / HMAC-SHA384（含恒定时间验证）、HKDF-Extract / HKDF-Expand（raw `info`，TLS 1.3 Expand-Label 由调用方在外层组合） |
| `aead` | AES-128-GCM / AES-256-GCM seal/open，nonce 由调用方给出，库不生成 |
| `ecdsa` | P-256 / P-384 签名与验签，DER 与 fixed-width 两种编码透出并可互转；私钥生成从宿主熵取标量，非法标量有限重试 |
| `rsa` | PKCS#1 v1.5 与 PSS 的 SHA-256 / SHA-384 验签；自有 OAEP(SHA-1) 填充加密（种子逐次取自宿主熵）。私钥生成与随机化签名需要外部 RNG trait，本 crate 不提供 |
| `x25519` | 密钥生成（宿主熵）、公钥派生、DH 共享秘密（返回 `SecretBytes`） |
| `encoding` | base64 标准填充与无填充两种编解码 |
| `secret` | `SecretBytes` 秘密字节生命周期类型 |

## 自有实现

下列能力已由本 crate 自行实现，不依赖外部 crate，并按标准已知答案向量验收：

| 能力 | 说明 |
|------|------|
| SHA-1 / SHA-224 / SHA-256 / SHA-384 / SHA-512 | FIPS 180-4 一次性摘要（`hash`） |
| HMAC-SHA256 | RFC 2104，含恒定时间标签比较（`mac`） |
| HKDF-SHA256 | RFC 5869 Extract / Expand（`mac`） |
| AES-128 / AES-256 与 GCM | FIPS 197 分组加密、NIST SP 800-38D 的 CTR 与 GHASH 认证（`aead`），S-box 由 GF(2^8) 逆元现算 |
| X25519 | RFC 7748 的 GF(2^255-19) 乘法、Montgomery ladder 与 clamp（`x25519`） |
| RSAES-OAEP(SHA-1) 填充 | RFC 8017，含 `MGF1`，模幂用 `rsa` 提供的 `BigUint`（`rsa`） |
| P-256 / P-384 ECDSA | NIST 曲线域运算、标量乘、ECDSA 签名/验签与 DER / fixed-width 编码，随机标量逐次取自 `boost-random`（`ecdsa`） |
| HMAC-SHA384 | RFC 2104，与 SHA-256 共用同一实现（`mac`） |

## 依赖台账

按外部依赖吸收准入逐条登记。许可证以 registry 实际元数据为准。

| crate | 版本 | 许可证 | 用途 |
|-------|------|--------|------|
| `rsa` | 0.9.10 | MIT OR Apache-2.0 | RSA 公钥解析、验签、`BigUint` 模幂与签名 trait 层 |
| `base64` | 0.23.1 | MIT OR Apache-2.0 | base64 标准与无填充编解码 |
| `zeroize` | 1.9.1 | Apache-2.0 OR MIT | 秘密材料清零（`derive` feature，`Zeroizing`/`ZeroizeOnDrop`） |
| `sha1-rsa`（package `sha1`） | 0.10 | MIT OR Apache-2.0 | OAEP(SHA-1) 的 `lHash` 与 MGF1 |
| `sha2-rsa`（package `sha2`） | 0.10.9 | MIT OR Apache-2.0 | 仅为 `rsa` 0.9 提供 digest 0.10 世代的摘要类型（`oid` feature） |

禁止项：`ring`、`aws-lc`、外部 OpenSSL/BoringSSL 及任何 C/汇编密码载荷。原语执行全部为纯 Rust。

## 验收

`tests/main.rs` 固定向量：SHA-256/384 NIST KAT、HMAC RFC 4231 case 1/2、HKDF RFC 5869 case 1、AES-128-GCM NIST CAVS（空 AAD 与带 AAD）、X25519 RFC 7748 §6.1；P-256/P-384 为 sign→verify 往返加篡改拒绝；RSA 用固定夹具验证 PKCS#1 v1.5 与 PSS 验签及篡改拒绝，并验证自有 OAEP(SHA-1) 密文可被夹具私钥解回且两次加密不同；`SecretBytes` 验证 Debug 打码与 drop 清零。
