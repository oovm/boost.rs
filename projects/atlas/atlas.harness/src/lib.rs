//! Atlas 云调用的录制、回放、根因诊断与稳定报告。
//!
//! 本 crate 只做云调用专用的观察与诊断，不重做 `boost.harness` 的测试运行器：
//! 用例编排、断言与结果投影仍由调用方自己的测试框架负责。
//!
//! # 两件事
//!
//! - **简化测试**：[`Replay`] 是 [`boost_net::http::Transport`] 的脚本化替身，直接交给
//!   `atlas-cloud` 的客户端，就能在不联网、不读环境变量的前提下走完「构造 payload →
//!   签名 → 解析响应 → 结果分层」。
//! - **根因查找**：[`diagnose_error`]、[`diagnose_sms`] 与 [`diagnose_cos`] 把结果映射成
//!   [`Diagnosis`]，明确指出哪一层出错、是否可能已生效、能不能重试、下一步查什么。
//!
//! # 示例
//!
//! ```no_run
//! use atlas_cloud::tencent::{
//!     Credentials, Region,
//!     sms::{Client, SendSms, SmsOutcome, Template},
//! };
//! use atlas_harness::{CannedResponse, Replay};
//!
//! # fn main() -> Result<(), atlas_cloud::tencent::CloudError> {
//! let replay = Replay::new(vec![CannedResponse::json(
//!     200,
//!     r#"{"Response":{"SendStatusSet":[{"Code":"Ok","Message":"send success","SerialNo":"5000:1"}],"RequestId":"rid-1"}}"#,
//! )]);
//! let client =
//!     Client::new(Credentials::new("AKIDEXAMPLE", "secretkey"), "1400000000", Region::ap_guangzhou())
//!         .with_transport(Box::new(replay.clone()));
//!
//! let outcome = client.send(&SendSms {
//!     phones: vec!["+8613800138000".to_string()],
//!     sign_name: "示例签名".to_string(),
//!     template: Template::new("1234567").param("654321"),
//! })?;
//! assert!(matches!(outcome, SmsOutcome::Sent { .. }));
//! assert!(replay.last_request().is_some_and(|request| request.header("authorization").is_some()));
//! # Ok(())
//! # }
//! ```
#![warn(missing_docs)]

mod diagnosis;
mod error;
mod replay;
mod report;

pub use diagnosis::{Diagnosis, Layer, diagnose_cos, diagnose_error, diagnose_sms};
pub use error::HarnessError;
pub use replay::{CannedResponse, RecordedRequest, Replay};
pub use report::{Record, Report};
