//! Atlas 首批云服务：腾讯云 SMS 与 COS 的作者面。
//!
//! 本 crate 只依赖 `boost-net`（HTTP/1.1 与 TLS）、`boost-crypto`（摘要、HMAC 与编码）与
//! `serde_json`，不引入厂商 SDK，也不建立厂商无关的统一抽象：对外只有 [`tencent::sms`]
//! 与 [`tencent::cos`] 两个腾讯云原生命名空间。
//!
//! # 结果语义
//!
//! 三个层次不得互相折叠：
//!
//! - 本地配置缺失 → [`tencent::CloudError::InvalidConfig`]，请求没有发出。
//! - 服务端明确拒绝 → [`tencent::CloudError::Rejected`] 或各结果枚举的 `Rejected`，
//!   附厂商 [`tencent::RequestId`]。
//! - 无法判定是否生效（连接失败、超时、读响应中断）→ [`tencent::CloudError::Indeterminate`]
//!   或各结果枚举的 `Indeterminate`。这一层可能已经发送并计费、或已经写入对象存储，
//!   调用方**不得**自动重试。
//!
//! 客户端默认单次发送，不内建重试循环；重试策略由调用方按每个操作的幂等性决定。
//!
//! ```no_run
//! use atlas_cloud::tencent::{
//!     CloudError, Credentials, Region,
//!     sms::{Client, SendSms, SmsOutcome, Template},
//! };
//!
//! fn send() -> Result<(), CloudError> {
//!     let sms = Client::new(Credentials::from_env()?, "1400000000", Region::ap_guangzhou());
//!     let outcome = sms.send(&SendSms {
//!         phones: vec!["+8613800138000".into()],
//!         sign_name: "示例签名".into(),
//!         template: Template::new("1234567").param("123456"),
//!     })?;
//!     match outcome {
//!         SmsOutcome::Sent { serial_no, request_id } => println!("{serial_no} {request_id}"),
//!         SmsOutcome::Rejected { code, .. } => println!("拒绝：{code}"),
//!         SmsOutcome::Indeterminate { reason, .. } => println!("结果未知：{reason}"),
//!     }
//!     Ok(())
//! }
//! ```
#![deny(missing_docs)]

pub mod tencent;
